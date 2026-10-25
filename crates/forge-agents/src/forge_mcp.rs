//! Forge's own MCP server, given to every agent session that takes HTTP MCP servers. Its
//! tools hand work back to Forge's UI instead of doing it behind the user's back:
//! `propose_commit` and `propose_push` show the commit or the push in the thread, and the
//! user makes it from Forge.
//!
//! One server per process, on 127.0.0.1 with a random port; each thread registers a
//! secret path (`/mcp/<token>`) and receives its tool calls on a channel. The transport is
//! MCP's Streamable HTTP with plain JSON answers: a tool call's request stays open until
//! the user decides.
//!
//! The project's skills (`library`) are tools too, answered here from their files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use futures::channel::{mpsc, oneshot};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

/// The server's name as agents see it (Claude Code calls its tools `mcp__forge__*`).
pub const SERVER_NAME: &str = "forge";

/// A tool call for the thread that registered the token: the tool's name and arguments,
/// and where its answer goes.
pub(crate) struct ToolRequest {
    pub name: String,
    pub args: Value,
    pub reply: oneshot::Sender<ToolReply>,
}

/// What the tool answers the agent: text, and whether it is an error.
pub(crate) type ToolReply = Result<String, String>;

/// A thread's place: where its tool calls go, and its project (for the project's skills).
#[derive(Clone)]
struct Session {
    tx: mpsc::UnboundedSender<ToolRequest>,
    root: PathBuf,
    /// Set once the agent has listed the tools: it can call them from then on.
    listed: Arc<AtomicBool>,
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

struct Server {
    port: u16,
    sessions: Sessions,
}

static SERVER: OnceLock<Option<Server>> = OnceLock::new();

/// A thread's place on the server; dropping it closes the place.
pub(crate) struct Registration {
    token: String,
    url: String,
    sessions: Sessions,
    listed: Arc<AtomicBool>,
}

impl Registration {
    /// The thread's URL on the server.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether the agent has listed the tools since [`Registration::expect_listing`]: agents
    /// connect to MCP servers in the background, and a prompt sent before this runs without
    /// Forge's tools. Shared so a thread can wait for it off the UI thread.
    pub fn listed(&self) -> Arc<AtomicBool> {
        self.listed.clone()
    }

    /// An agent is starting: it has to list the tools again.
    pub fn expect_listing(&self) {
        self.listed.store(false, Ordering::SeqCst);
    }

    /// The ACP `McpServer` object to pass in `session/new`.
    pub fn acp_server(&self) -> Value {
        json!({ "type": "http", "name": SERVER_NAME, "url": self.url, "headers": [] })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.sessions.lock().unwrap().remove(&self.token);
    }
}

/// Registers a thread working in `root`, starting the server the first time. `None` when
/// the server couldn't start (agents then work without Forge's tools).
pub(crate) fn register(_runtime: &tokio::runtime::Handle, root: PathBuf) -> Option<(Registration, mpsc::UnboundedReceiver<ToolRequest>)> {
    let server = SERVER.get_or_init(|| match start() {
        Ok(server) => Some(server),
        Err(e) => {
            log::error!("Forge's MCP server couldn't start: {e:#}");
            None
        }
    });
    let server = server.as_ref()?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let (tx, rx) = mpsc::unbounded();
    let listed = Arc::new(AtomicBool::new(false));
    server.sessions.lock().unwrap().insert(token.clone(), Session { tx, root, listed: listed.clone() });
    let url = format!("http://127.0.0.1:{}/mcp/{token}", server.port);
    Some((Registration { token, url, sessions: server.sessions.clone(), listed }, rx))
}

/// Serves on a thread and runtime of its own: the server outlives whichever runtime
/// registered first (each test has its own, which ends with the test).
fn start() -> anyhow::Result<Server> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let sessions: Sessions = Default::default();
    let served = sessions.clone();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    std::thread::Builder::new().name("forge-mcp".into()).spawn(move || runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(e) => return log::error!("Forge's MCP server: {e}"),
        };
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let sessions = served.clone();
                    tokio::spawn(async move {
                        match serve(stream, sessions).await {
                            // Agents close connections whenever they like.
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof) => log::debug!("Forge's MCP server: {e}"),
                            Err(e) => log::warn!("Forge's MCP server: {e}"),
                            Ok(()) => {}
                        }
                    });
                }
                Err(e) => log::warn!("Forge's MCP server: {e}"),
            }
        }
    }))?;
    Ok(Server { port, sessions })
}

/// The largest request body taken (tool arguments are small).
const MAX_BODY: usize = 16 * 1024 * 1024;

/// An HTTP request as the server needs it.
#[derive(Debug, Default, PartialEq)]
struct HttpRequest {
    method: String,
    path: String,
    /// The `Origin` header, sent by browsers (and so by web pages trying to reach the server).
    origin: Option<String>,
    body: Vec<u8>,
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_string())
}

/// Reads one request; `None` when the connection closes between requests. Bodies come
/// with a `Content-Length` or chunked.
async fn read_request<R: tokio::io::AsyncBufRead + Unpin>(read: &mut R) -> std::io::Result<Option<HttpRequest>> {
    let mut request_line = String::new();
    if read.read_line(&mut request_line).await? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let mut request = HttpRequest { method: parts.next().unwrap_or_default().to_string(), path: parts.next().unwrap_or_default().to_string(), ..Default::default() };
    let (mut length, mut chunked) = (0usize, false);
    loop {
        let mut header = String::new();
        if read.read_line(&mut header).await? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else { continue };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().map_err(|_| invalid("bad Content-Length"))?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        } else if name.eq_ignore_ascii_case("origin") {
            request.origin = Some(value.to_string());
        }
    }
    if chunked {
        loop {
            let mut size = String::new();
            read.read_line(&mut size).await?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or_default(), 16).map_err(|_| invalid("bad chunk size"))?;
            if request.body.len() + size > MAX_BODY {
                return Err(invalid("request too large"));
            }
            let start = request.body.len();
            request.body.resize(start + size, 0);
            read.read_exact(&mut request.body[start..]).await?;
            // The CRLF after the chunk (after the trailers, for the last one).
            let mut end = String::new();
            read.read_line(&mut end).await?;
            if size == 0 {
                while !end.trim_end().is_empty() {
                    end.clear();
                    if read.read_line(&mut end).await? == 0 {
                        break;
                    }
                }
                break;
            }
        }
    } else {
        if length > MAX_BODY {
            return Err(invalid("request too large"));
        }
        request.body = vec![0; length];
        read.read_exact(&mut request.body).await?;
    }
    Ok(Some(request))
}

/// Requests from web pages are refused (MCP asks servers to check `Origin`, against DNS
/// rebinding): only local origins, or none (agents send none).
fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let host = origin.split("://").nth(1).unwrap_or(origin);
    let host = host.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(host, |(host, _)| host);
    origin == "null" || matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// One connection: HTTP/1.1 requests one after another (keep-alive).
async fn serve(stream: tokio::net::TcpStream, sessions: Sessions) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    loop {
        let Some(HttpRequest { method, path, origin, body }) = read_request(&mut read).await? else { return Ok(()) };
        let session = path.strip_prefix("/mcp/").and_then(|token| sessions.lock().unwrap().get(token).cloned());
        let (status, answer) = match (method.as_str(), session) {
            _ if !origin_allowed(origin.as_deref()) => ("403 Forbidden", None),
            (_, None) => ("404 Not Found", None),
            ("POST", Some(session)) => match serde_json::from_slice::<Value>(&body) {
                Ok(message) => match handle(message, &session).await {
                    Some(answer) => ("200 OK", Some(answer)),
                    None => ("202 Accepted", None),
                },
                Err(_) => ("400 Bad Request", Some(error(Value::Null, -32700, "parse error"))),
            },
            // No server-sent stream to open, and sessions end with the thread.
            ("DELETE", Some(_)) => ("200 OK", None),
            _ => ("405 Method Not Allowed", None),
        };
        let body = answer.map(|a| a.to_string()).unwrap_or_default();
        let content_type = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
        let head = format!("HTTP/1.1 {status}\r\n{content_type}Content-Length: {}\r\n\r\n", body.len());
        write.write_all(head.as_bytes()).await?;
        write.write_all(body.as_bytes()).await?;
        write.flush().await?;
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answers one JSON-RPC message; `None` for notifications.
async fn handle(message: Value, session: &Session) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            // Speak the client's version: the server only needs the basics of any of them.
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2025-06-18")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            "instructions": server_instructions(&session.root),
        }),
        "ping" => json!({}),
        "tools/list" => {
            session.listed.store(true, Ordering::SeqCst);
            json!({ "tools": all_tools(&session.root) })
        }
        "tools/call" => call(&params, session).await,
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Sent with the first message of a session (and given to agents that take instructions
/// another way, see [`crate::agent_setup`]). The server's own instructions are
/// [`BRIEF_INSTRUCTIONS`]: Claude Code cuts them at 2048 characters.
pub const INSTRUCTIONS: &str = "You are working inside Forge, an IDE the user is watching. The `forge` MCP server's tools act through it, \
so the user sees what you do; prefer them to doing the same in a shell:\n\
- Change files with your own edit and write tools, never through the shell (no `sed -i`, `echo >`, `cat >>`, heredocs or \
scripts that write files): the user reviews each change as a diff in Forge, and what a shell writes skips that review.\n\
- Read and search with Forge instead of shell commands; these need no permission and see unsaved edits: `read_file` for part of \
a file (from a line, or its last lines, instead of `head`, `tail` or `sed -n`), `search` for text or a regex across the project \
(instead of `grep`, `rg` or `find | xargs grep`; with globs, context lines, or only the file names), and `git_history` for the \
history (instead of `git log`, `git log -S`, `git show`, `git blame` and `git diff`).\n\
- Run commands with `run_command`: they run in a real terminal of Forge's whose output the user watches live, while your own shell \
tool only shows it when the command ends. Give it `tail`, `head` or `filter` (a regex) instead of piping into `tail`, `head` or \
`grep`: you get that part, the user still sees everything. Keep your shell for quick commands that print little (`git status`, `pwd`).\n\
- Commit with `propose_commit` and push with `propose_push`, never with `git commit` or `git push`: the user makes them from Forge.\n\
- Run tests with `run_tests` (they show in the Tests panel) instead of `cargo test`, `dotnet test`, `npm test`, `pytest` and the like.\n\
- After editing files, call `check_file`: it lists only the problems your changes brought, once the language servers have caught up. \
Fix them before moving on. `diagnostics` lists every error and warning (in a file, a folder or the files you changed); both are faster \
than building the whole project.\n\
- Navigate with `go_to_definition`, `find_references`, `workspace_symbols` and `hover` (types and docs); see a file's outline with \
`document_symbols` before reading it whole; find implementations with `go_to_implementation`, a value's type with `go_to_type_definition`, \
callers and callees with `call_hierarchy` (before changing a function), and what to pass a function with `signature_help`. Rename with `rename_symbol` \
instead of search and replace. Use the language server's fixes and refactors with `code_actions` + `apply_code_action`, and `format_file` to format.\n\
- Run the app with `run_app` (the user's Run button), read it with `app_output`, stop it with `stop_app`.\n\
- Call the app's HTTP endpoints with `http_request` (the user sees request and response in Forge) instead of curl.\n\
- To find out why something misbehaves, debug instead of adding prints: `set_breakpoint`, `start_debugging` (an app or tests), \
`debug_step`, `debug_evaluate`, `stop_debugging`. The user follows the session in the Debug panel.\n\
- Point the user at code with `show_file`, and at your changes with `show_changes`.\n\
- When the user says \"this\", \"here\" or \"the error\", call `user_context`: the file, line and selection they are on, their open files, \
the problems near their cursor, their terminal's last output and their failing tests.\n\
- When you learn something about this project that every future session should know (how to build or test it, a convention, \
a trap), propose it with `remember`: the user keeps it in the project's instructions.\n\
- To help set Forge up, look settings up with `forge_settings` and key bindings with `forge_keybindings`, how features work in \
`forge_guide`, and propose changes with `change_settings` and `change_keybinding` (the user applies them) instead of editing Forge's \
files yourself.\n\
- When you need the user to decide something to go on, ask with `ask_user` (it waits for the answer) instead of ending your turn; \
when a long task finishes, tell them with `notify`.";

/// The server's instructions (`initialize`): the gist of [`INSTRUCTIONS`], which come
/// whole with the first message. Kept well under the 2048 characters Claude Code keeps,
/// with room for the project's agent-ignore patterns and skills (see [`server_instructions`]).
pub const BRIEF_INSTRUCTIONS: &str = "Forge's tools act in the IDE the user is watching: prefer them to the shell. \
Read with `read_file`, search with `search` (text, regex, globs, `files_only`) and read the history with `git_history`, not with cat, \
head, tail, sed, grep, rg, find or git log/show/blame/diff. Change files with your own edit and write tools, never through the shell. \
Run commands with `run_command` (`tail`, `head` or `filter` instead of pipes) and tests with `run_tests`, not cargo/go/dotnet/npm test \
or pytest. After editing, call `check_file`. Commit with `propose_commit` and push with `propose_push`, never git commit or git push. \
There are tools to navigate and refactor code, debug, run the app, call HTTP endpoints and ask the user; the full guide comes with the \
user's first message.";

/// Added to Claude Code's system prompt, so it knows before its first command.
const SYSTEM_PROMPT_APPEND: &str = "You are running inside Forge, an IDE, whose `forge` MCP server reads, searches and runs things where \
the user sees them. This overrides any other guidance about using the shell to read or search: don't use the shell to read, search or \
write files, or to read git history (no cat, head, tail, sed, awk, grep, rg, git grep, find, git log, git show, git blame, git diff, no \
redirecting into files); use your own Read, Edit and Write tools and `mcp__forge__read_file`, `mcp__forge__search` (`files_only` to find \
files) and `mcp__forge__git_history`, which see unsaved edits. Run commands with `mcp__forge__run_command`, with `tail`, `head` or `filter` \
instead of pipes; tests with `mcp__forge__run_tests`, not cargo test, go test, dotnet test, npm test or pytest. Commit with \
`mcp__forge__propose_commit` and push with `mcp__forge__propose_push`: Forge refuses git commit and git push. Edit files with your own Edit \
and Write tools rather than other MCP servers' tools that write files: those skip the user's review in Forge. Forge runs a plain `cat`, \
`head`, `tail`, `sed -n`, `grep`, `rg`, `git log`, `git show`, `git blame` or `git diff`, a test command, and a command piped only into \
`grep`, `head` or `tail`, through its tools anyway, answering in their format, and refuses the rest, saying which tool to use: calling the \
tools yourself saves the step.";

/// The `_meta` of every session: Claude Code's adapter reads it, Forge's agy adapter reads
/// `forge.shellHook`, Codex's `jetbrains.air.customInstructions` (other agents ignore it).
/// It tells Claude, in its system prompt, to read, search and write files with Forge's
/// tools and its own instead of the shell, and installs a hook that runs the simple shell
/// commands that do through Forge's tools and refuses the rest (see
/// [`crate::shell_guard`]), with the tool to use instead; `url` is the thread's server, which
/// the hook calls. It also
/// asks for summaries of Claude's thinking: recent models otherwise send it empty, and
/// the thread only says "Working".
pub fn session_meta(url: Option<&str>) -> Value {
    let mut options = json!({ "extraArgs": { "thinking-display": "summarized" } });
    let mut meta = json!({ "systemPrompt": { "append": SYSTEM_PROMPT_APPEND } });
    if let Ok(exe) = std::env::current_exe() {
        options["settings"] = crate::shell_guard::hook_settings(&exe, url);
        // Forge's own adapters (agy's) install it as their agent's hook.
        meta["forge"] = json!({ "shellHook": { "command": crate::shell_guard::hook_command(&exe) } });
    }
    meta["claudeCode"] = json!({ "options": options });
    // Codex's adapter makes these developer instructions (it reads them where JetBrains AIR
    // puts its own; whether the client is AIR it tells from `initialize`, not from here).
    meta["jetbrains"] = json!({ "air": { "customInstructions": INSTRUCTIONS } });
    meta
}

/// `path` + `line` + `symbol`: where a symbol is, for the language-server tools (and its
/// new name, to rename it).
fn symbol_schema(rename: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
            "line": { "type": "integer", "minimum": 1, "description": "The line the symbol is on (1-based)." },
            "symbol": { "type": "string", "description": "The symbol's name, as written on that line." }
        },
        "required": ["path", "line", "symbol"]
    });
    if rename {
        schema["properties"]["new_name"] = json!({ "type": "string", "description": "The new name." });
        schema["required"] = json!(["path", "line", "symbol", "new_name"]);
    }
    schema
}

/// `path` + `line` (+ `end_line`): some lines, for code actions (and an action's `title`).
fn lines_schema(apply: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
            "line": { "type": "integer", "minimum": 1, "description": "First line (1-based); for a problem's fix, its line." },
            "end_line": { "type": "integer", "minimum": 1, "description": "Last line, when more than one." }
        },
        "required": ["path", "line"]
    });
    if apply {
        schema["properties"]["title"] = json!({ "type": "string", "description": "The action's exact title, as `code_actions` listed it." });
        schema["required"] = json!(["path", "line", "title"]);
    }
    schema
}

/// What Forge tells agents about its tools, with the tools extensions add and the skills
/// of the project in `root`.
pub fn instructions(root: &Path) -> String {
    let mut text = INSTRUCTIONS.to_string();
    let extension_tools = forge_ui::agent_tools::agent_tools().list();
    if !extension_tools.is_empty() {
        let listed: Vec<String> = extension_tools.iter().map(|t| format!("`{}` ({}, from the {} extension)", t.name, t.title, t.extension)).collect();
        text.push_str(&format!("\n- The user's extensions add tools to the `forge` server too: {}. Prefer them for what they cover.", listed.join(", ")));
    }
    text.push_str(&project_instructions(root));
    text
}

/// The server's instructions: [`BRIEF_INSTRUCTIONS`], then what is particular to the
/// project (files kept from agents, skills), which the agent needs before its first call.
fn server_instructions(root: &Path) -> String {
    format!("{BRIEF_INSTRUCTIONS}{}", project_instructions(root))
}

/// The files the project keeps from agents, and its skills.
fn project_instructions(root: &Path) -> String {
    let mut text = String::new();
    let kept = crate::agent_ignore::AgentIgnore::load(root);
    text.push_str(&format!(
        "\n- Don't read, search or change files matching these patterns (`.gitignore` syntax): the user keeps them from agents. \
Forge refuses them; ask the user if you need something from one: {}.",
        kept.patterns().join(", ")
    ));
    let nested: Vec<String> = crate::agent_ignore::files().into_iter().filter(|f| !f.contains('/')).map(|f| format!("`{f}`")).collect();
    if !nested.is_empty() {
        text.push_str(&format!(" A {} file in any folder adds patterns for the files below it, the same way.", nested.join(" or ")));
    }
    let skills = crate::library::skills(root);
    if !skills.is_empty() {
        let listed: Vec<String> = skills.iter().map(|s| format!("`{}` ({})", crate::library::tool_name(&s.name), s.description)).collect();
        text.push_str(&format!(
            "\n- The user wrote skills for this project, as `forge` tools: {}. When a task matches one, call it first and follow the instructions it returns.",
            listed.join(", ")
        ));
    }
    text
}

/// Forge's tools, the project's skills and the tools extensions offer (`forge.agents.registerTool`).
fn all_tools(root: &Path) -> Value {
    let mut tools = tools();
    if let Value::Array(list) = &mut tools {
        // The tools Forge's other crates register (Tests, Run, the HTTP client).
        for tool in forge_ui::ide_tools::ide_tools().list() {
            let mut entry = json!({ "name": tool.name, "title": tool.title, "description": tool.description, "inputSchema": tool.input_schema });
            if tool.effect == forge_ui::ide_tools::ToolEffect::Reads {
                entry["annotations"] = json!({ "readOnlyHint": true });
            }
            list.push(entry);
        }
        for tool in list.iter_mut().filter(|t| t["name"].as_str().is_some_and(|n| ALWAYS_LOADED.contains(&n))) {
            tool["_meta"] = json!({ "anthropic/alwaysLoad": true });
        }
        for skill in crate::library::skills(root) {
            list.push(json!({
                "name": crate::library::tool_name(&skill.name),
                "title": format!("Skill: {}", skill.name),
                "description": format!("{} (a skill the user wrote: call it to get its instructions, then follow them)", skill.description),
                "inputSchema": { "type": "object", "properties": {} },
                "annotations": { "readOnlyHint": true },
            }));
        }
        for tool in forge_ui::agent_tools::agent_tools().list() {
            let mut description = format!("{} (from the {} extension", tool.description, tool.extension);
            description.push_str(if tool.read_only { ")" } else { "; the user may be asked first)" });
            let mut entry = json!({ "name": tool.name, "title": tool.title, "description": description, "inputSchema": tool.input_schema });
            if tool.read_only {
                entry["annotations"] = json!({ "readOnlyHint": true });
            }
            list.push(entry);
        }
    }
    tools
}

/// The tools that stand in for the shell's commands: Claude Code loads them upfront
/// instead of behind its tool search, so using them costs the agent no more than its shell.
const ALWAYS_LOADED: &[&str] = &["read_file", "search", "git_history", "run_command", "run_tests", "check_file", "propose_commit"];

/// Forge's own tools, as `tools/list` shows them.
fn tools() -> Value {
    TOOLS.iter().map(|t| json!({ "name": t.name, "title": t.title, "description": t.description, "inputSchema": (t.schema)() })).collect()
}

/// One of Forge's own tools: what agents see of it, and its arguments' JSON schema.
struct ToolDef {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: fn() -> Value,
}

/// A tool without arguments.
fn no_args() -> Value {
    json!({ "type": "object", "properties": {} })
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "user_context",
        title: "What the user is looking at",
        description: "What the user is working on right now in Forge: the file and line they are on and what they selected, the files they have open, \
the problems the language servers report near their cursor, the end of the terminal they last used, and the tests that failed in their last run. \
Call it when they refer to \"this\", \"here\", \"the error\" or \"the failing test\".",
        schema: no_args,
    },
    ToolDef {
        name: "remember",
        title: "Remember for the project",
        description: "Propose a note for the project's standing instructions (its AGENTS.md), which agents get at the start of every session: how to build \
or test it, a convention, a trap you ran into. One short, self-contained note per call. The user may edit it, and keeps it or not; returns which.",
        schema: || json!({ "type": "object", "properties": { "note": { "type": "string", "description": "The note, in a sentence or two, as an instruction." } }, "required": ["note"] }),
    },
    ToolDef {
        name: "ask_user",
        title: "Ask the user",
        description: "Ask the user a question in the conversation and wait for the answer, instead of ending your turn to ask. \
Give `options` when the answer is one of a few choices (the user clicks one, or writes something else). Returns the answer, \
or that the user skipped the question (then decide yourself and say what you chose).",
        schema: || json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question, short and self-contained." },
                "options": { "type": "array", "items": { "type": "string" }, "maxItems": 6, "description": "Possible answers, most likely first." }
            },
            "required": ["question"]
        }),
    },
    ToolDef {
        name: "notify",
        title: "Notify the user",
        description: "Tell the user something worth their attention now (a long task finished, something needs them) with a notification in Forge. \
Doesn't wait for anything.",
        schema: || json!({ "type": "object", "properties": { "message": { "type": "string", "description": "One short sentence." } }, "required": ["message"] }),
    },
    ToolDef {
        name: "propose_commit",
        title: "Propose a commit",
        description: "Propose a git commit to the user instead of running `git commit`. Forge shows the message and the files in the conversation; \
the user commits them from Forge, edits the message in the Git panel, or declines. Waits for that decision and returns the new commit's SHA and \
final message, or that the user declined. Don't stage files yourself: list them in `files`. Don't write Signed-off-by lines: Forge signs the \
commit off as the user when the repository asks for it.",
        schema: || json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The commit message: a subject line, then optionally a blank line and a body. Follow the repository's style." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Files to commit, relative to the project root (or absolute). Leave it out to commit every change." }
            },
            "required": ["message"]
        }),
    },
    ToolDef {
        name: "propose_push",
        title: "Propose a push",
        description: "Propose pushing the current branch to the user instead of running `git push`. Forge shows the branch, where it goes and the commits \
it takes; the user pushes or declines. Pushes go to the branch's upstream, or create the branch on the remote and make it the upstream. Never forced. \
Waits for that decision and returns the result.",
        schema: || json!({
            "type": "object",
            "properties": {
                "remote": { "type": "string", "description": "The remote to push to. Leave it out for the branch's upstream (or `origin` for a new branch)." }
            }
        }),
    },
    ToolDef {
        name: "diagnostics",
        title: "Errors and warnings",
        description: "The errors and warnings the language servers report, as the user sees them in the editor (unsaved edits included): \
in a file or a folder, in the files you changed in this conversation, or in every file that has some. Waits for the servers to catch up \
after recent edits. Faster than building the project.",
        schema: || json!({ "type": "object", "properties": {
            "path": { "type": "string", "description": "Only this file, or the files in this folder (relative to the project root)." },
            "severity": { "enum": ["error", "warning"], "description": "Only errors, or only warnings. Leave it out for both." },
            "changed": { "type": "boolean", "description": "Only the files you changed in this conversation." }
        } }),
    },
    ToolDef {
        name: "check_file",
        title: "Check your changes",
        description: "After editing: the problems your changes brought to a file (or to every file you changed in this conversation), \
once the language servers have looked at the new code. Problems the file already had before you changed it are left out (only counted), \
so what it lists is yours to fix. Call it after each round of edits, before moving on.",
        schema: || json!({ "type": "object", "properties": { "path": { "type": "string", "description": "The file (relative to the project root). Leave it out for every file you changed." } } }),
    },
    ToolDef {
        name: "go_to_definition",
        title: "Go to definition",
        description: "Where a symbol is defined, according to the language server: file, line, column and that line's code.",
        schema: || symbol_schema(false),
    },
    ToolDef {
        name: "find_references",
        title: "Find references",
        description: "Every place a symbol is used, according to the language server (more precise than text search): file, line, column and code.",
        schema: || symbol_schema(false),
    },
    ToolDef {
        name: "rename_symbol",
        title: "Rename a symbol",
        description: "Rename a symbol everywhere it is used, with the language server's rename (like the editor's), instead of search and replace. \
The change goes through the editor and into the conversation's changes, which the user reviews; the user may be asked first. Returns the files changed.",
        schema: || symbol_schema(true),
    },
    ToolDef {
        name: "hover",
        title: "Type and docs",
        description: "What the language server says about a symbol: its type or signature and its documentation.",
        schema: || symbol_schema(false),
    },
    ToolDef {
        name: "document_symbols",
        title: "File outline",
        description: "A file's outline from its language server: its types, functions, methods and fields, nested, with their lines. \
Read it before reading a large file whole, to go straight to the part you need.",
        schema: || json!({ "type": "object", "properties": { "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." } }, "required": ["path"] }),
    },
    ToolDef {
        name: "go_to_implementation",
        title: "Go to implementations",
        description: "Where an interface, a trait or an abstract/virtual method is implemented, according to the language server.",
        schema: || symbol_schema(false),
    },
    ToolDef {
        name: "go_to_type_definition",
        title: "Go to type definition",
        description: "Where the type of a variable, parameter or expression is defined (not the variable itself).",
        schema: || symbol_schema(false),
    },
    ToolDef {
        name: "call_hierarchy",
        title: "Call hierarchy",
        description: "Who calls a function or method (`incoming`, the default), or what it calls (`outgoing`), with where: to see what a change \
to it affects before making it.",
        schema: || json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "The line the function's name is on (1-based)." },
                "symbol": { "type": "string", "description": "The function's name, as written on that line." },
                "direction": { "enum": ["incoming", "outgoing"] }
            },
            "required": ["path", "line", "symbol"]
        }),
    },
    ToolDef {
        name: "signature_help",
        title: "Signature help",
        description: "The signatures (overloads) and parameter docs of the function called on a line, as `symbol(…)`: what to pass it.",
        schema: || symbol_schema(false),
    },
    ToolDef {
        name: "workspace_symbols",
        title: "Find symbols",
        description: "Types, functions and other symbols whose name matches, across the project, with where they are (the language servers' workspace symbols).",
        schema: || json!({ "type": "object", "properties": { "query": { "type": "string", "description": "A name or part of it." } }, "required": ["query"] }),
    },
    ToolDef {
        name: "code_actions",
        title: "Code actions",
        description: "The language server's code actions on some lines: quick fixes for the problems there, imports to add, refactors. \
Apply one with `apply_code_action`.",
        schema: || lines_schema(false),
    },
    ToolDef {
        name: "apply_code_action",
        title: "Apply a code action",
        description: "Apply one of the code actions `code_actions` listed for those lines, by its exact title. The change goes through the editor \
into the conversation's changes; the user may be asked first. Returns the files changed.",
        schema: || lines_schema(true),
    },
    ToolDef {
        name: "format_file",
        title: "Format a file",
        description: "Format a file with the project's formatter (as the editor's Format does). The change joins the conversation's changes.",
        schema: || json!({ "type": "object", "properties": { "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." } }, "required": ["path"] }),
    },
    ToolDef {
        name: "set_breakpoint",
        title: "Set a breakpoint",
        description: "Set a breakpoint at a line (replacing one already there), optionally only when a condition holds. \
It shows in the editor's gutter; running debug sessions pick it up. Returns the breakpoints set.",
        schema: || json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "The line (1-based)." },
                "condition": { "type": "string", "description": "Stop only when this expression (in the program's language) is true." }
            },
            "required": ["path", "line"]
        }),
    },
    ToolDef {
        name: "remove_breakpoint",
        title: "Remove a breakpoint",
        description: "Remove the breakpoint at a line, or every breakpoint with `all`.",
        schema: || json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "The line (1-based)." },
                "all": { "type": "boolean", "description": "Remove every breakpoint." }
            }
        }),
    },
    ToolDef {
        name: "start_debugging",
        title: "Start debugging",
        description: "Debug a run target (like the title bar's Debug) or tests (like a test's Debug button), with the breakpoints set. \
Builds, starts, and waits until it stops at a breakpoint (up to a minute): returns where (stack) and the local variables, \
or that it ended without stopping. Give `test_path` or `test_name` for tests, else `target` (or nothing, for the selected target).",
        schema: || json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The run target's name." },
                "test_path": { "type": "string", "description": "Debug the tests in this file or folder." },
                "test_name": { "type": "string", "description": "Debug the tests whose name contains this." }
            }
        }),
    },
    ToolDef {
        name: "debug_step",
        title: "Continue or step",
        description: "Move the paused program on and wait until it stops again: `continue` (to the next breakpoint), `step_over`, `step_in`, \
`step_out`, or `pause` a running program. Returns the new stack and local variables, or that it ended.",
        schema: || json!({
            "type": "object",
            "properties": { "action": { "type": "string", "enum": ["continue", "step_over", "step_in", "step_out", "pause"], "description": "Default: continue." } }
        }),
    },
    ToolDef {
        name: "debug_evaluate",
        title: "Evaluate an expression",
        description: "Evaluate an expression in the paused program's current frame (as the debug console does) and return its value and type.",
        schema: || json!({ "type": "object", "properties": { "expression": { "type": "string" } }, "required": ["expression"] }),
    },
    ToolDef {
        name: "stop_debugging",
        title: "Stop debugging",
        description: "End the debug session.",
        schema: no_args,
    },
    ToolDef {
        name: "show_file",
        title: "Show a file to the user",
        description: "Open a file in the user's editor at a line (or a range of lines, selected), to point them at something you are talking about.",
        schema: || json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "First line to show (1-based)." },
                "end_line": { "type": "integer", "minimum": 1, "description": "Last line of the range to select." }
            },
            "required": ["path"]
        }),
    },
    ToolDef {
        name: "show_changes",
        title: "Show your changes",
        description: "Open the review of the files you changed in this conversation as diffs (where the user keeps or undoes each change), at one file if given.",
        schema: || json!({ "type": "object", "properties": { "path": { "type": "string", "description": "The file to show first." } } }),
    },
    ToolDef {
        name: "read_file",
        title: "Read a file",
        description: "Reads lines of a file, numbered, as Forge has it (unsaved edits included): from `start_line`, or the last `tail` \
lines. Use it instead of `head`, `tail`, `sed -n` or `cat` in a shell. Without `lines` it reads 250 lines.",
        schema: || json!({ "type": "object", "properties": {
            "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
            "start_line": { "type": "integer", "minimum": 1, "description": "The first line to read (1-based)." },
            "lines": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "How many lines (default 250)." },
            "tail": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "Read the last this many lines instead." }
        }, "required": ["path"] }),
    },
    ToolDef {
        name: "search",
        title: "Search the project",
        description: "Searches the project's files for text or a regex with Forge's search (the Search panel's): unsaved edits included, \
files git ignores and the ones kept from agents left out. Answers `path:line: text` like grep. Use it instead of `grep`, `rg` or `find` in a shell.",
        schema: || json!({ "type": "object", "properties": {
            "query": { "type": "string", "description": "The text to find, or a regex when `regex` is true." },
            "regex": { "type": "boolean", "description": "`query` is a regular expression (default false)." },
            "case_sensitive": { "type": "boolean", "description": "Default false." },
            "whole_word": { "type": "boolean", "description": "Default false." },
            "include": { "type": ["string", "array"], "items": { "type": "string" }, "description": "Only files matching these globs (comma-separated or a list), e.g. `*.rs` or `src/**`." },
            "exclude": { "type": ["string", "array"], "items": { "type": "string" }, "description": "Leave out files matching these globs." },
            "context": { "type": "integer", "minimum": 0, "maximum": 10, "description": "Lines shown around each match (default 0)." },
            "files_only": { "type": "boolean", "description": "List only the matching files, with how many lines match." },
            "max_results": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Stop after this many matching lines (default 100)." }
        }, "required": ["query"] }),
    },
    ToolDef {
        name: "git_history",
        title: "Search git history",
        description: "Reads the repository's history, without changing anything. `action`: `log` (commits, of `path` if given, following \
renames; filter by `author`, `since`, `message`), `changes` (commits that added or removed `text`, or whose changed lines match `regex`; \
`patch` for their diffs), `show` (a `commit`: message and diff, of `path` if given; `stat_only` for the files), `file_at` (`path` as it \
was at `commit`), `blame` (who last changed each line of `path`, `start_line`-`end_line`), `diff` (between `from` and `to`, or `from` and \
the working tree, or the uncommitted changes; `stat_only` for the files). Use it instead of git commands in a shell.",
        schema: || json!({ "type": "object", "properties": {
            "action": { "type": "string", "enum": ["log", "changes", "show", "file_at", "blame", "diff"], "description": "Default `log`." },
            "path": { "type": "string", "description": "A file or folder, relative to the project root." },
            "commit": { "type": "string", "description": "A commit, branch or tag (`show`, `file_at`)." },
            "text": { "type": "string", "description": "`changes`: text added or removed (git log -S)." },
            "regex": { "type": "string", "description": "`changes`: a regex the changed lines match (git log -G)." },
            "author": { "type": "string" },
            "since": { "type": "string", "description": "A date, or `2 weeks ago`." },
            "message": { "type": "string", "description": "`log`/`changes`: text in the commit message." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Commits listed at most (default 20)." },
            "patch": { "type": "boolean", "description": "`changes`: include each commit's diff." },
            "stat": { "type": "boolean", "description": "`log`: list the files each commit changed." },
            "stat_only": { "type": "boolean", "description": "`show`/`diff`: only the files and their changed line counts." },
            "start_line": { "type": "integer", "minimum": 1 },
            "end_line": { "type": "integer", "minimum": 1 },
            "from": { "type": "string", "description": "`diff`: the commit to compare from." },
            "to": { "type": "string", "description": "`diff`: the commit to compare to (default: the working tree)." }
        } }),
    },
    ToolDef {
        name: "run_command",
        title: "Run a command",
        description: "Runs a shell command in a terminal of Forge's that the user watches live in this conversation, and returns its exit \
code and output (the end of it, if long) once it ends. Use it for builds, installs, scripts, migrations and anything that takes a while \
(your own shell tool only shows the user the output at the end). The user's permission settings apply, as for your other commands.",
        schema: || json!({ "type": "object", "properties": {
            "command": { "type": "string", "description": "The command line, run by the user's shell." },
            "cwd": { "type": "string", "description": "Where to run it, relative to the project root (default: the root)." },
            "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "Stop it after this long (default 600)." },
            "tail": { "type": "integer", "minimum": 1, "description": "Pass on only the last this many lines of the output (instead of `| tail`)." },
            "head": { "type": "integer", "minimum": 1, "description": "Pass on only the first this many lines of the output (instead of `| head`)." },
            "filter": { "type": "string", "description": "Pass on only the output lines matching this regex (instead of `| grep`); with `tail` or `head`, of those." }
        }, "required": ["command"] }),
    },
    ToolDef {
        name: "forge_settings",
        title: "Forge's settings",
        description: "Looks up Forge's settings (the editor's, the terminal's, git, languages, .NET, agents, each extension's…), as the Settings tab \
has them: each one's key, description, type or allowed values, default and current value, and the file it is in. Search by words \
(`font size terminal`), or list a whole page with `page`. Use it before proposing changes with `change_settings`.",
        schema: || json!({ "type": "object", "properties": {
            "query": { "type": "string", "description": "Words that the setting's key, title or description contain." },
            "page": { "type": "string", "description": "Only this page (its id, as results show it)." }
        } }),
    },
    ToolDef {
        name: "change_settings",
        title: "Change settings",
        description: "Proposes changes to Forge's settings: the user sees each one (before → after) and applies or discards them. They take \
effect at once. `key` as `forge_settings` shows it; `value: null` puts the default back. Agents' own setup (which agents run, their \
permissions and MCP servers) is the user's to change: don't propose it.",
        schema: || json!({ "type": "object", "properties": {
            "changes": { "type": "array", "items": { "type": "object", "properties": {
                "key": { "type": "string" },
                "value": { "description": "The new value, of the setting's type; null for the default." },
                "page": { "type": "string", "description": "The page, when the key is on several." }
            }, "required": ["key", "value"] } }
        }, "required": ["changes"] }),
    },
    ToolDef {
        name: "forge_keybindings",
        title: "Key bindings",
        description: "What keys do in Forge (and in which context), or which keys run an action: give `keys` (`cmd-shift-p`, `cmd-k cmd-t`; \
`secondary` is cmd on macOS and ctrl elsewhere) or words of an action's name (`action`: `minimap`, `terminal::Toggle`). Actions without \
keys are listed too.",
        schema: || json!({ "type": "object", "properties": { "keys": { "type": "string" }, "action": { "type": "string" } } }),
    },
    ToolDef {
        name: "change_keybinding",
        title: "Change a key binding",
        description: "Proposes a key binding for the user's keymap.json: `keys` run `action` (with `args`, if it takes any) where `context` \
holds (`Editor`, `Workspace`, `Terminal`; everywhere when left out), or do nothing there with `action: null`. The user applies or \
discards it; it takes effect at once.",
        schema: || json!({ "type": "object", "properties": {
            "keys": { "type": "string" },
            "action": { "type": ["string", "null"], "description": "The action's name, as `forge_keybindings` shows it; null to unbind." },
            "args": { "description": "The action's arguments, for actions that take some." },
            "context": { "type": "string" }
        }, "required": ["keys", "action"] }),
    },
    ToolDef {
        name: "forge_guide",
        title: "Forge's guide",
        description: "Forge's own documentation: how its features work and where they are set up (agents, threads, tests, debugging, git, \
.NET, HTTP files, extensions…). Without `topic`, its sections; with one, the sections about it.",
        schema: || json!({ "type": "object", "properties": { "topic": { "type": "string" } } }),
    },
];

/// The title of one of Forge's tools, from its name as agents call it
/// (`mcp__forge__run_tests` or `run_tests`).
pub fn tool_title(name: &str) -> Option<String> {
    let name = name.strip_prefix(&format!("mcp__{SERVER_NAME}__")).unwrap_or(name);
    if let Some(skill) = name.strip_prefix("skill_") {
        return Some(format!("Skill: {skill}"));
    }
    if let Some(tool) = forge_ui::agent_tools::agent_tools().get(name) {
        return Some(tool.title);
    }
    if let Some(tool) = forge_ui::ide_tools::ide_tools().get(name) {
        return Some(tool.title.to_string());
    }
    TOOLS.iter().find(|t| t.name == name).map(|t| t.title.to_string())
}

/// The arguments of a call to one of Forge's tools, in a few words: `src/a.rs:12 parse`.
pub fn tool_summary(args: &Value) -> Option<String> {
    let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let mut parts = Vec::new();
    if let Some(path) = text("path") {
        parts.push(match args.get("line").and_then(Value::as_u64) {
            Some(line) => format!("{path}:{line}"),
            None => path.to_string(),
        });
    }
    if let (Some(method), Some(url)) = (text("method"), text("url")) {
        parts.push(format!("{method} {url}"));
    } else if let Some(url) = text("url") {
        parts.push(url.to_string());
    }
    for key in ["symbol", "name", "target", "remote", "query", "title", "test_path", "test_name", "action", "expression", "keys", "topic", "page"] {
        parts.extend(text(key).map(str::to_string));
    }
    if let Some(new_name) = text("new_name") {
        parts.push(format!("→ {new_name}"));
    }
    if let Some(condition) = text("condition") {
        parts.push(format!("if {condition}"));
    }
    if let Some(message) = text("message") {
        parts.push(message.lines().next().unwrap_or_default().to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

async fn call(params: &Value, session: &Session) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let known = all_tools(&session.root).as_array().is_some_and(|ts| ts.iter().any(|t| t["name"] == name.as_str()));
    let skill = name.starts_with("skill_").then(|| crate::library::skill_for_tool(&session.root, &name)).flatten();
    let reply = if let Some(skill) = skill {
        Ok(crate::library::skill_reply(&skill))
    } else if !known {
        Err(format!("Unknown tool: {name}"))
    } else {
        let (reply, answer) = oneshot::channel();
        match session.tx.unbounded_send(ToolRequest { name, args, reply }) {
            Ok(()) => answer.await.unwrap_or_else(|_| Err("The conversation was closed before the tool answered.".into())),
            Err(_) => Err("The conversation is closed.".into()),
        }
    };
    let (text, is_error) = match reply {
        Ok(text) => (text, false),
        Err(text) => (text, true),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    async fn post(url: &str, body: Value) -> (String, String) {
        let rest = url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_at(rest.find('/').unwrap());
        let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
        let body = body.to_string();
        let request = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).await.unwrap();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).await.unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some(v) = header.to_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
        (status.trim().to_string(), String::from_utf8(body).unwrap())
    }

    async fn parse(raw: &str) -> std::io::Result<Option<HttpRequest>> {
        read_request(&mut BufReader::new(raw.as_bytes())).await
    }

    #[tokio::test]
    async fn reads_requests_with_a_length_or_chunked() {
        let plain = parse("POST /mcp/t HTTP/1.1\r\nContent-Length: 2\r\nOrigin: http://localhost:3000\r\n\r\n{}").await.unwrap().unwrap();
        assert_eq!((plain.method.as_str(), plain.path.as_str(), plain.body.as_slice(), plain.origin.as_deref()), ("POST", "/mcp/t", &b"{}"[..], Some("http://localhost:3000")));
        let chunked = parse("POST /mcp/t HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3;x=y\r\n:1}\r\n0\r\nTrailer: z\r\n\r\n").await.unwrap().unwrap();
        assert_eq!(chunked.body, b"{\"a\":1}");
        assert_eq!(parse("").await.unwrap(), None, "closed between requests");
        assert!(parse("POST / HTTP/1.1\r\nContent-Length: 999999999999\r\n\r\n").await.is_err(), "too large");
        assert!(parse("POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n").await.is_err());
    }

    #[test]
    fn tool_list_is_unchanged() {
        let expected: Value = serde_json::from_str(include_str!("../tests/fixtures/forge_tools.json")).unwrap();
        assert_eq!(tools(), expected, "Forge's tools differ from tests/fixtures/forge_tools.json");
        let mut names = std::collections::HashSet::new();
        for tool in TOOLS {
            assert!(names.insert(tool.name), "{} is listed twice", tool.name);
            assert!(!tool.description.trim().is_empty() && !tool.title.is_empty(), "{} needs a title and a description", tool.name);
            let schema = (tool.schema)();
            assert!(schema["type"] == "object" && schema["properties"].is_object(), "{}'s schema isn't an object", tool.name);
        }
    }

    #[test]
    fn refuses_web_pages() {
        assert!(origin_allowed(None), "agents send no Origin");
        assert!(origin_allowed(Some("http://localhost:5173")) && origin_allowed(Some("http://127.0.0.1")) && origin_allowed(Some("null")));
        assert!(!origin_allowed(Some("https://evil.example")));
        assert!(!origin_allowed(Some("http://localhost.evil.example")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_tools_and_waits_for_the_user() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".forge/skills")).unwrap();
        std::fs::write(project.path().join(".forge/skills/migrations.md"), "---\ndescription: Add a database migration\n---\nRun `make migration NAME=…`.").unwrap();
        let (registration, mut requests) = register(&tokio::runtime::Handle::current(), project.path().to_path_buf()).unwrap();
        let url = registration.acp_server()["url"].as_str().unwrap().to_string();

        let (status, body) = post(&url, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } })).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        let init: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "forge");
        let server_instructions = init["result"]["instructions"].as_str().unwrap();
        assert!(server_instructions.contains("`skill_migrations` (Add a database migration)"));
        assert!(server_instructions.starts_with(BRIEF_INSTRUCTIONS), "the gist, not the whole guide");
        assert!(server_instructions.len() < 2048, "Claude Code cuts them at 2048 characters: {}", server_instructions.len());

        let (status, _) = post(&url, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!(status, "HTTP/1.1 202 Accepted");
        assert!(!registration.listed().load(Ordering::SeqCst), "not until the tools are listed");

        let (_, body) = post(&url, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        assert!(registration.listed().load(Ordering::SeqCst), "the agent can call the tools now");
        registration.expect_listing();
        assert!(!registration.listed().load(Ordering::SeqCst), "a restarted agent lists them again");
        let tools = &serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"];
        // Forge's own (tests elsewhere may register extensions' tools, named `<extension>__<tool>`).
        // This crate's own (tests elsewhere may register extensions' tools, named
        // `<extension>__<tool>`, and the tools of Forge's other crates).
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .filter(|n| !n.contains("__") && !n.starts_with("skill_") && forge_ui::ide_tools::ide_tools().get(n).is_none())
            .collect();
        assert_eq!(names, [
            "user_context", "remember", "ask_user", "notify", "propose_commit", "propose_push", "diagnostics", "check_file", "go_to_definition", "find_references", "rename_symbol", "hover",
            "document_symbols", "go_to_implementation", "go_to_type_definition", "call_hierarchy", "signature_help", "workspace_symbols", "code_actions", "apply_code_action", "format_file", "set_breakpoint", "remove_breakpoint",
            "start_debugging", "debug_step", "debug_evaluate", "stop_debugging", "show_file", "show_changes", "read_file", "search", "git_history", "run_command", "forge_settings", "change_settings",
            "forge_keybindings", "change_keybinding", "forge_guide",
        ]);
        let rename = tools.as_array().unwrap().iter().find(|t| t["name"] == "rename_symbol").unwrap();
        assert_eq!(rename["inputSchema"]["required"], json!(["path", "line", "symbol", "new_name"]));
        // `run_tests` too, once the Tests panel registers it.
        let always_loaded: Vec<&str> = tools.as_array().unwrap().iter().filter(|t| t["_meta"]["anthropic/alwaysLoad"] == true).map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(always_loaded, ["propose_commit", "check_file", "read_file", "search", "git_history", "run_command"]);
        let skill = tools.as_array().unwrap().iter().find(|t| t["name"] == "skill_migrations").expect("the project's skill");
        assert_eq!(skill["title"], "Skill: migrations");

        // A skill answers with its instructions, without the thread.
        let (_, body) = post(&url, json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "skill_migrations", "arguments": {} } })).await;
        let text = serde_json::from_str::<Value>(&body).unwrap()["result"]["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.starts_with("Follow the skill \"migrations\"") && text.ends_with("Run `make migration NAME=…`."), "{text}");

        // The call waits until the thread answers.
        let call = tokio::spawn({
            let url = url.clone();
            async move { post(&url, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "propose_commit", "arguments": { "message": "Fix the parser", "files": ["src/a.rs"] } } })).await }
        });
        let Some(ToolRequest { name, args, reply }) = requests.next().await else { panic!("no commit request") };
        assert_eq!((name.as_str(), &args), ("propose_commit", &json!({ "message": "Fix the parser", "files": ["src/a.rs"] })));
        reply.send(Ok("Committed abc1234.".into())).unwrap();
        let (_, body) = call.await.unwrap();
        let result = &serde_json::from_str::<Value>(&body).unwrap()["result"];
        assert_eq!(result["content"][0]["text"], "Committed abc1234.");
        assert_eq!(result["isError"], false);

        let push = tokio::spawn({
            let url = url.clone();
            async move { post(&url, json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "propose_push", "arguments": {} } })).await }
        });
        let Some(ToolRequest { name, reply, .. }) = requests.next().await else { panic!("no push request") };
        assert_eq!(name, "propose_push");
        reply.send(Err("The user declined the push.".into())).unwrap();
        let (_, body) = push.await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["isError"], true);

        // Unknown tools never reach the thread.
        let (_, body) = post(&url, json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "format_disk", "arguments": {} } })).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["isError"], true);

        // Closed threads and unknown tokens are not found.
        drop(registration);
        let (status, _) = post(&url, json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" })).await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
    }
}

