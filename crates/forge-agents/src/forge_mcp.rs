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
}

impl Registration {
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

/// Registers a thread working in `root`, starting the server on `runtime` the first time.
/// `None` when the server couldn't start (agents then work without Forge's tools).
pub(crate) fn register(runtime: &tokio::runtime::Handle, root: PathBuf) -> Option<(Registration, mpsc::UnboundedReceiver<ToolRequest>)> {
    let server = SERVER.get_or_init(|| match start(runtime) {
        Ok(server) => Some(server),
        Err(e) => {
            log::error!("Forge's MCP server couldn't start: {e:#}");
            None
        }
    });
    let server = server.as_ref()?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let (tx, rx) = mpsc::unbounded();
    server.sessions.lock().unwrap().insert(token.clone(), Session { tx, root });
    let url = format!("http://127.0.0.1:{}/mcp/{token}", server.port);
    Some((Registration { token, url, sessions: server.sessions.clone() }, rx))
}

fn start(runtime: &tokio::runtime::Handle) -> anyhow::Result<Server> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let sessions: Sessions = Default::default();
    let served = sessions.clone();
    runtime.spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(e) => return log::error!("Forge's MCP server: {e}"),
        };
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let sessions = served.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve(stream, sessions).await {
                            log::debug!("Forge's MCP server: {e}");
                        }
                    });
                }
                Err(e) => log::warn!("Forge's MCP server: {e}"),
            }
        }
    });
    Ok(Server { port, sessions })
}

/// One connection: HTTP/1.1 requests one after another (keep-alive).
async fn serve(stream: tokio::net::TcpStream, sessions: Sessions) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    loop {
        let mut request_line = String::new();
        if read.read_line(&mut request_line).await? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if read.read_line(&mut header).await? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0; length];
        read.read_exact(&mut body).await?;

        let session = path.strip_prefix("/mcp/").and_then(|token| sessions.lock().unwrap().get(token).cloned());
        let (status, answer) = match (method.as_str(), session) {
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
            "instructions": instructions(&session.root),
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": all_tools(&session.root) }),
        "tools/call" => call(&params, session).await,
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Shown to the agent when it connects, and sent with the first message of a session.
pub const INSTRUCTIONS: &str = "You are working inside Forge, an IDE the user is watching. The `forge` MCP server's tools act through it, \
so the user sees what you do; prefer them to doing the same in a shell:\n\
- Commit with `propose_commit` and push with `propose_push`, never with `git commit` or `git push`: the user makes them from Forge.\n\
- Run tests with `run_tests` (they show in the Tests panel) instead of `cargo test`, `dotnet test`, `npm test`, `pytest` and the like.\n\
- Check for errors with `diagnostics` (the language servers' errors and warnings, unsaved edits included) before building the whole project.\n\
- Navigate with `go_to_definition`, `find_references`, `workspace_symbols` and `hover` (types and docs), and rename with `rename_symbol` \
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
- When you need the user to decide something to go on, ask with `ask_user` (it waits for the answer) instead of ending your turn; \
when a long task finishes, tell them with `notify`.";

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

fn tools() -> Value {
    let none = json!({ "type": "object", "properties": {} });
    json!([{
        "name": "user_context",
        "title": "What the user is looking at",
        "description": "What the user is working on right now in Forge: the file and line they are on and what they selected, the files they have open, \
the problems the language servers report near their cursor, the end of the terminal they last used, and the tests that failed in their last run. \
Call it when they refer to \"this\", \"here\", \"the error\" or \"the failing test\".",
        "inputSchema": { "type": "object", "properties": {} }
    }, {
        "name": "remember",
        "title": "Remember for the project",
        "description": "Propose a note for the project's standing instructions (its AGENTS.md), which agents get at the start of every session: how to build \
or test it, a convention, a trap you ran into. One short, self-contained note per call. The user may edit it, and keeps it or not; returns which.",
        "inputSchema": { "type": "object", "properties": { "note": { "type": "string", "description": "The note, in a sentence or two, as an instruction." } }, "required": ["note"] }
    }, {
        "name": "ask_user",
        "title": "Ask the user",
        "description": "Ask the user a question in the conversation and wait for the answer, instead of ending your turn to ask. \
Give `options` when the answer is one of a few choices (the user clicks one, or writes something else). Returns the answer, \
or that the user skipped the question (then decide yourself and say what you chose).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question, short and self-contained." },
                "options": { "type": "array", "items": { "type": "string" }, "maxItems": 6, "description": "Possible answers, most likely first." }
            },
            "required": ["question"]
        }
    }, {
        "name": "notify",
        "title": "Notify the user",
        "description": "Tell the user something worth their attention now (a long task finished, something needs them) with a notification in Forge. \
Doesn't wait for anything.",
        "inputSchema": { "type": "object", "properties": { "message": { "type": "string", "description": "One short sentence." } }, "required": ["message"] }
    }, {
        "name": "propose_commit",
        "title": "Propose a commit",
        "description": "Propose a git commit to the user instead of running `git commit`. Forge shows the message and the files in the conversation; \
the user commits them from Forge, edits the message in the Git panel, or declines. Waits for that decision and returns the new commit's SHA and \
final message, or that the user declined. Don't stage files yourself: list them in `files`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The commit message: a subject line, then optionally a blank line and a body. Follow the repository's style." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Files to commit, relative to the project root (or absolute). Leave it out to commit every change." }
            },
            "required": ["message"]
        }
    }, {
        "name": "propose_push",
        "title": "Propose a push",
        "description": "Propose pushing the current branch to the user instead of running `git push`. Forge shows the branch, where it goes and the commits \
it takes; the user pushes or declines. Pushes go to the branch's upstream, or create the branch on the remote and make it the upstream. Never forced. \
Waits for that decision and returns the result.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "remote": { "type": "string", "description": "The remote to push to. Leave it out for the branch's upstream (or `origin` for a new branch)." }
            }
        }
    }, {
        "name": "run_tests",
        "title": "Run tests",
        "description": "Run tests in Forge's Tests panel (.NET, Go, Rust, Jest, Vitest, pytest), where the user sees them run and their results in the editor. \
Returns how many passed, failed and were skipped, and each failure with its file, line, message and stack; or the build output when nothing could run. \
With no arguments it runs every test. Waits for the run to end.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Only the tests in this file, or in the files under this folder (relative to the project root)." },
                "name": { "type": "string", "description": "Only the tests whose name (with its class and namespace) contains this, ignoring case." }
            }
        }
    }, {
        "name": "diagnostics",
        "title": "Errors and warnings",
        "description": "The errors and warnings the language servers report, as the user sees them in the editor (unsaved edits included): \
in one file, or in every file that has some. Waits for the servers to catch up after recent edits. Faster than building the project.",
        "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "Only this file (relative to the project root). Leave it out for every file." } } }
    }, {
        "name": "go_to_definition",
        "title": "Go to definition",
        "description": "Where a symbol is defined, according to the language server: file, line, column and that line's code.",
        "inputSchema": symbol_schema(false)
    }, {
        "name": "find_references",
        "title": "Find references",
        "description": "Every place a symbol is used, according to the language server (more precise than text search): file, line, column and code.",
        "inputSchema": symbol_schema(false)
    }, {
        "name": "rename_symbol",
        "title": "Rename a symbol",
        "description": "Rename a symbol everywhere it is used, with the language server's rename (like the editor's), instead of search and replace. \
The change goes through the editor and into the conversation's changes, which the user reviews; the user may be asked first. Returns the files changed.",
        "inputSchema": symbol_schema(true)
    }, {
        "name": "hover",
        "title": "Type and docs",
        "description": "What the language server says about a symbol: its type or signature and its documentation.",
        "inputSchema": symbol_schema(false)
    }, {
        "name": "workspace_symbols",
        "title": "Find symbols",
        "description": "Types, functions and other symbols whose name matches, across the project, with where they are (the language servers' workspace symbols).",
        "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A name or part of it." } }, "required": ["query"] }
    }, {
        "name": "code_actions",
        "title": "Code actions",
        "description": "The language server's code actions on some lines: quick fixes for the problems there, imports to add, refactors. \
Apply one with `apply_code_action`.",
        "inputSchema": lines_schema(false)
    }, {
        "name": "apply_code_action",
        "title": "Apply a code action",
        "description": "Apply one of the code actions `code_actions` listed for those lines, by its exact title. The change goes through the editor \
into the conversation's changes; the user may be asked first. Returns the files changed.",
        "inputSchema": lines_schema(true)
    }, {
        "name": "format_file",
        "title": "Format a file",
        "description": "Format a file with the project's formatter (as the editor's Format does). The change joins the conversation's changes.",
        "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." } }, "required": ["path"] }
    }, {
        "name": "run_app",
        "title": "Run the app",
        "description": "Start one of the project's run targets as the title bar's Run button does (a .NET app or Aspire host, a Cargo binary, a Go main package, \
a package.json script, a Python program), in a terminal the user sees. Returns its first seconds of output; then use `app_output` and `stop_app`. \
One thing runs at a time.",
        "inputSchema": { "type": "object", "properties": { "target": { "type": "string", "description": "The target's name. Leave it out for the one selected in the title bar; a wrong name lists them." } } }
    }, {
        "name": "app_output",
        "title": "App output",
        "description": "The latest output of what `run_app` (or the user's Run button) started, and whether it still runs.",
        "inputSchema": { "type": "object", "properties": { "lines": { "type": "integer", "minimum": 1, "description": "How many of the last lines (default 80)." } } }
    }, {
        "name": "stop_app",
        "title": "Stop the app",
        "description": "Stop what `run_app` (or the user's Run button) started.",
        "inputSchema": none
    }, {
        "name": "http_request",
        "title": "Send an HTTP request",
        "description": "Send an HTTP request through Forge's .http support, instead of curl: the user sees it and its response in Forge. \
Either the request at `line` of a `.http` file, or an ad-hoc one (`method`, `url`, `headers`, `body`). `{{variables}}` resolve with the project's \
http-client.env.json environment the user selected. Returns status, headers, timing and body.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A .http file with the request (relative to the project root)." },
                "line": { "type": "integer", "minimum": 1, "description": "A line of the request in that file (1-based)." },
                "method": { "type": "string", "description": "For an ad-hoc request; default GET." },
                "url": { "type": "string", "description": "For an ad-hoc request; may use {{variables}}." },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "description": "Text, or JSON to send as is." }
            }
        }
    }, {
        "name": "set_breakpoint",
        "title": "Set a breakpoint",
        "description": "Set a breakpoint at a line (replacing one already there), optionally only when a condition holds. \
It shows in the editor's gutter; running debug sessions pick it up. Returns the breakpoints set.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "The line (1-based)." },
                "condition": { "type": "string", "description": "Stop only when this expression (in the program's language) is true." }
            },
            "required": ["path", "line"]
        }
    }, {
        "name": "remove_breakpoint",
        "title": "Remove a breakpoint",
        "description": "Remove the breakpoint at a line, or every breakpoint with `all`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "The line (1-based)." },
                "all": { "type": "boolean", "description": "Remove every breakpoint." }
            }
        }
    }, {
        "name": "start_debugging",
        "title": "Start debugging",
        "description": "Debug a run target (like the title bar's Debug) or tests (like a test's Debug button), with the breakpoints set. \
Builds, starts, and waits until it stops at a breakpoint (up to a minute): returns where (stack) and the local variables, \
or that it ended without stopping. Give `test_path` or `test_name` for tests, else `target` (or nothing, for the selected target).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The run target's name." },
                "test_path": { "type": "string", "description": "Debug the tests in this file or folder." },
                "test_name": { "type": "string", "description": "Debug the tests whose name contains this." }
            }
        }
    }, {
        "name": "debug_step",
        "title": "Continue or step",
        "description": "Move the paused program on and wait until it stops again: `continue` (to the next breakpoint), `step_over`, `step_in`, \
`step_out`, or `pause` a running program. Returns the new stack and local variables, or that it ended.",
        "inputSchema": {
            "type": "object",
            "properties": { "action": { "type": "string", "enum": ["continue", "step_over", "step_in", "step_out", "pause"], "description": "Default: continue." } }
        }
    }, {
        "name": "debug_evaluate",
        "title": "Evaluate an expression",
        "description": "Evaluate an expression in the paused program's current frame (as the debug console does) and return its value and type.",
        "inputSchema": { "type": "object", "properties": { "expression": { "type": "string" } }, "required": ["expression"] }
    }, {
        "name": "stop_debugging",
        "title": "Stop debugging",
        "description": "End the debug session.",
        "inputSchema": none
    }, {
        "name": "show_file",
        "title": "Show a file to the user",
        "description": "Open a file in the user's editor at a line (or a range of lines, selected), to point them at something you are talking about.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root (or absolute)." },
                "line": { "type": "integer", "minimum": 1, "description": "First line to show (1-based)." },
                "end_line": { "type": "integer", "minimum": 1, "description": "Last line of the range to select." }
            },
            "required": ["path"]
        }
    }, {
        "name": "show_changes",
        "title": "Show your changes",
        "description": "Open the review of the files you changed in this conversation as diffs (where the user keeps or undoes each change), at one file if given.",
        "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "The file to show first." } } }
    }])
}

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
    tools().as_array()?.iter().find(|t| t["name"] == name)?["title"].as_str().map(str::to_string)
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
    for key in ["symbol", "name", "target", "remote", "query", "title", "test_path", "test_name", "action", "expression"] {
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
        assert!(init["result"]["instructions"].as_str().unwrap().contains("`skill_migrations` (Add a database migration)"));

        let (status, _) = post(&url, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!(status, "HTTP/1.1 202 Accepted");

        let (_, body) = post(&url, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        let tools = &serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"];
        // Forge's own (tests elsewhere may register extensions' tools, named `<extension>__<tool>`).
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).filter(|n| !n.contains("__") && !n.starts_with("skill_")).collect();
        assert_eq!(names, [
            "user_context", "remember", "ask_user", "notify", "propose_commit", "propose_push", "run_tests", "diagnostics", "go_to_definition", "find_references", "rename_symbol", "hover",
            "workspace_symbols", "code_actions", "apply_code_action", "format_file", "run_app", "app_output", "stop_app", "http_request", "set_breakpoint", "remove_breakpoint",
            "start_debugging", "debug_step", "debug_evaluate", "stop_debugging", "show_file", "show_changes",
        ]);
        assert_eq!(tools[10]["inputSchema"]["required"], json!(["path", "line", "symbol", "new_name"]));
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

