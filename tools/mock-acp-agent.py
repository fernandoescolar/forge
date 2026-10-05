#!/usr/bin/env python3
"""Tiny line-delimited JSON-RPC ACP agent for local end-to-end testing.

Prompts it understands:
  read <path>          -> fs/read_text_file and reply with the content
  write <path> <text>  -> session/request_permission (showing a diff), then fs/write_text_file
  edit <path> <text>   -> fs/write_text_file straight away (the client may review it)
  read <path> [line]   -> a read tool call with its location, then fs/read_text_file
  run <shell command>  -> session/request_permission, then terminal/create + wait + output
  anything else        -> echoed back as an agent_message_chunk

With MOCK_REQUIRE_AUTH=1 the agent behaves like one that needs a login: it offers a
terminal auth method (this script with --login), reports "Not logged in" and refuses
prompts until the marker file (MOCK_AUTH_MARKER, default $TMPDIR/forge-mock-auth) exists.
"""
import json, os, sys, tempfile, uuid

REQUIRE_AUTH = os.environ.get("MOCK_REQUIRE_AUTH") == "1"
AUTH_MARKER = os.environ.get("MOCK_AUTH_MARKER") or os.path.join(tempfile.gettempdir(), "forge-mock-auth")


def authed():
    return not REQUIRE_AUTH or os.path.exists(AUTH_MARKER)


if len(sys.argv) > 1 and sys.argv[1] == "--login":
    print("Mock agent login")
    answer = input("Press Enter to sign in (type 'no' to cancel): ").strip().lower()
    if answer == "no":
        print("Cancelled.")
        sys.exit(1)
    with open(AUTH_MARKER, "w") as f:
        f.write("ok")
    print("Signed in!")
    sys.exit(0)

sessions = {}
client_capabilities = {}
next_id = 0
cancelled = set()


def send(x):
    sys.stdout.write(json.dumps(x, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def say(sid, text, kind="agent_message_chunk"):
    if kind == "agent_message_chunk" and sid in sessions:
        sessions[sid]["history"].append(("agent", text))
    notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}})


def call(method, params):
    """Send a request to the client and block until its response arrives."""
    global next_id
    next_id += 1
    rid = f"agent-{next_id}"
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except Exception:
            continue
        if msg.get("id") == rid and "method" not in msg:
            if "error" in msg:
                raise RuntimeError(msg["error"].get("message"))
            return msg.get("result")
        if msg.get("method") == "session/cancel":
            cancelled.add((msg.get("params") or {}).get("sessionId"))
    raise SystemExit(0)


def abs_path(sid, path):
    return path if os.path.isabs(path) else os.path.join(sessions[sid]["cwd"], path)


def ask_permission(sid, tool_call):
    """Returns "allow", "reject" or "cancelled"."""
    result = call("session/request_permission", {"sessionId": sid, "toolCall": tool_call, "options": [
        {"optionId": "allow", "name": "Yes", "kind": "allow_once"},
        {"optionId": "allow_always", "name": "Yes, and don't ask again this session", "kind": "allow_always"},
        {"optionId": "reject", "name": "No", "kind": "reject_once"}]})
    outcome = result["outcome"]
    if outcome.get("outcome") == "cancelled" or sid in cancelled:
        cancelled.discard(sid)
        return "cancelled"
    return "allow" if outcome.get("optionId") == "allow_always" else outcome.get("optionId")


def config_options(session):
    """The session settings, like Claude's adapter lists them (ACP configOptions)."""
    return [
        {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": session["mode"],
         "options": [{"value": m["id"], "name": m["name"], "description": m["description"]} for m in MODES]},
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": session.get("model", "sonnet"),
         "options": [{"value": "sonnet", "name": "Sonnet", "description": "Fast"}, {"value": "opus", "name": "Opus", "description": "Most capable"}]},
    ]


# Session modes, like Claude's adapter offers them.
MODES = [{"id": "default", "name": "Manual", "description": "Always ask before making changes"},
         {"id": "auto", "name": "Auto", "description": "Run what is judged safe without asking"},
         {"id": "acceptEdits", "name": "Accept edits", "description": "Automatically accept all file edits"},
         {"id": "plan", "name": "Plan", "description": "Create a plan before making changes"}]


def update_tool(sid, tool_call_id, **fields):
    notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, **fields}})


def run_command(sid, command):
    if not client_capabilities.get("terminal"):
        say(sid, "This client does not support terminals.")
        return "end_turn"
    tool_call_id = "call-" + uuid.uuid4().hex[:6]
    tool_call = {"toolCallId": tool_call_id, "title": f"Run `{command}`", "kind": "execute", "status": "pending"}
    notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "tool_call", **tool_call}})
    decision = ask_permission(sid, tool_call)
    if decision == "cancelled":
        return "cancelled"
    if decision != "allow":
        update_tool(sid, tool_call_id, status="failed")
        say(sid, "Not running it.")
        return "end_turn"
    term = call("terminal/create", {"sessionId": sid, "command": "sh", "args": ["-c", command],
                                    "cwd": sessions[sid]["cwd"], "outputByteLimit": 100000})["terminalId"]
    update_tool(sid, tool_call_id, status="in_progress", content=[{"type": "terminal", "terminalId": term}])
    exit_status = call("terminal/wait_for_exit", {"sessionId": sid, "terminalId": term})
    output = call("terminal/output", {"sessionId": sid, "terminalId": term})["output"]
    call("terminal/release", {"sessionId": sid, "terminalId": term})
    code = exit_status.get("exitCode")
    update_tool(sid, tool_call_id, status="completed" if code == 0 else "failed")
    say(sid, f"Output:\n```\n{output.strip()}\n```\nExit code {code}")
    return "end_turn"


USAGE_MARKDOWN = """## Usage

> Claude Max subscription usage

### Limits

**5-hour limit** — **42%** · Resets Oct 3, 5:00 PM GMT+2

`████████░░░░░░░░░░░░`

**Weekly · all models** — **18%** · Resets Oct 7, 9:00 AM GMT+2

`████░░░░░░░░░░░░░░░░`
"""


def prompt(sid, text):
    if text.strip() == "/usage":
        # Like Claude's adapter: a local command, answered without the model.
        say(sid, USAGE_MARKDOWN)
        return "end_turn"
    words = text.split(" ", 2)
    if words[0] == "run" and len(text) > 4:
        return run_command(sid, text[4:])
    if words[0] == "read" and len(words) >= 2:
        say(sid, "Let me look at that file.", "agent_thought_chunk")
        # Like real agents, say where it is reading (clients follow along).
        location = {"path": abs_path(sid, words[1])}
        if len(words) == 3 and words[2].isdigit():
            location["line"] = int(words[2])
        tool_call_id = "call-" + uuid.uuid4().hex[:6]
        notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "tool_call", "toolCallId": tool_call_id, "title": f"Read {words[1]}",
                                                               "kind": "read", "status": "completed", "locations": [location]}})
        content = call("fs/read_text_file", {"sessionId": sid, "path": abs_path(sid, words[1])})["content"]
        say(sid, f"Read: {content}")
        return "end_turn"
    if words[0] == "edit" and len(words) == 3:
        try:
            call("fs/write_text_file", {"sessionId": sid, "path": abs_path(sid, words[1]), "content": words[2] + "\n"})
            say(sid, f"Edited {words[1]}.")
        except RuntimeError as e:
            say(sid, f"Edit refused: {e}")
        return "end_turn"
    if words[0] == "write" and len(words) == 3:
        tool_call_id = "call-" + uuid.uuid4().hex[:6]
        path = abs_path(sid, words[1])
        try:
            with open(path) as f:
                old_text = f.read()
        except OSError:
            old_text = None
        tool_call = {"toolCallId": tool_call_id, "title": f"Write {words[1]}", "kind": "edit", "status": "pending",
                     "locations": [{"path": path}],
                     "content": [{"type": "diff", "path": path, "oldText": old_text, "newText": words[2]}]}
        notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "tool_call", **tool_call}})
        decision = ask_permission(sid, tool_call)
        if decision == "cancelled":
            return "cancelled"
        if decision != "allow":
            update_tool(sid, tool_call_id, status="failed")
            say(sid, "Permission denied, not writing.")
            return "end_turn"
        call("fs/write_text_file", {"sessionId": sid, "path": abs_path(sid, words[1]), "content": words[2]})
        notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, "status": "completed"}})
        say(sid, f"Wrote {words[1]}.")
        return "end_turn"
    say(sid, f"Echo: {text}")
    return "end_turn"


for line in sys.stdin:
    try:
        msg = json.loads(line)
    except Exception:
        continue
    rid = msg.get("id"); method = msg.get("method"); params = msg.get("params") or {}
    try:
        if method == "initialize":
            client_capabilities = params.get("clientCapabilities") or {}
            auth_methods = []
            if REQUIRE_AUTH and (client_capabilities.get("_meta") or {}).get("terminal-auth"):
                auth_methods = [{"id": "mock-login", "name": "Mock account", "description": "Sign in to the mock agent",
                                 "type": "terminal", "args": ["--login"],
                                 "_meta": {"terminal-auth": {"command": sys.executable, "args": [os.path.abspath(__file__), "--login"], "label": "Mock login",
                                                                 "env": {"MOCK_AUTH_MARKER": AUTH_MARKER}}}}]
            result = {"echoClientCapabilities": client_capabilities, "protocolVersion": params.get("protocolVersion", 1), "agentInfo": {"name": "Forge Mock ACP", "version": "0.2"},
                      "agentCapabilities": {"loadSession": True, "promptCapabilities": {"image": True}}, "authMethods": auth_methods}
        elif method == "session/new":
            sid = "mock-" + uuid.uuid4().hex[:10]
            sessions[sid] = {"cwd": params.get("cwd") or os.getcwd(), "history": []}
            sessions[sid]["mode"] = "default"
            result = {"sessionId": sid, "echoMcpServers": params.get("mcpServers") or [],
                      "modes": {"currentModeId": "default", "availableModes": MODES}, "configOptions": config_options(sessions[sid])}
            notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "usage", "description": "Show plan usage limits", "input": None}]}})
            if not authed():
                notify("_auth/status_update", {"authStatus": {"kind": "none", "label": "Not logged in"}})
        elif method == "session/prompt":
            sid = params.get("sessionId")
            if sid not in sessions:
                raise ValueError(f"unknown session {sid}")
            if not authed():
                raise ValueError("Authentication required")
            blocks = params.get("prompt") or []
            text = next((p.get("text", "") for p in blocks if p.get("type") == "text"), "")
            context = [p.get("name") or p.get("uri") for p in blocks if p.get("type") == "resource_link"]
            context += [p["resource"].get("uri") for p in blocks if p.get("type") == "resource"]
            sessions[sid]["history"].append(("user", text))
            if context:
                say(sid, "Context: " + ", ".join(context) + "\n\n")
            if any(p.get("type") == "text" and p.get("text", "").startswith("Standing instructions") for p in blocks[1:]):
                say(sid, "Instructions received.\n\n")
            images = [p for p in blocks if p.get("type") == "image" and p.get("data")]
            if images:
                say(sid, f"Images: {len(images)} ({images[0].get('mimeType')})\n\n")
            stop = prompt(sid, text)
            # Token accounting like real agents report it (ACP session usage): the context
            # grows with the conversation; each turn reports its own tokens.
            turn_in, turn_out = 1200 + 4 * len(text), 300
            session = sessions[sid]
            session["used"] = session.get("used", 8000) + turn_in + turn_out
            session["cost"] = session.get("cost", 0.0) + 0.003
            notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "usage_update", "used": session["used"], "size": 200000,
                                                                   "cost": {"amount": round(session["cost"], 4), "currency": "USD"}}})
            result = {"stopReason": stop, "usage": {"totalTokens": turn_in + turn_out, "inputTokens": turn_in, "outputTokens": turn_out, "cachedReadTokens": 800}}
        elif method == "session/load":
            sid = params.get("sessionId")
            if sid not in sessions:
                raise ValueError(f"unknown session {sid}")
            for role, text in sessions[sid]["history"]:
                kind = "user_message_chunk" if role == "user" else "agent_message_chunk"
                notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}})
            result = None
        elif method == "authenticate":
            with open(AUTH_MARKER, "w") as f:
                f.write("ok")
            result = {}
        elif method == "session/set_mode":
            sid = params.get("sessionId")
            sessions[sid]["mode"] = params.get("modeId")
            notify("session/update", {"sessionId": sid, "update": {"sessionUpdate": "current_mode_update", "currentModeId": params.get("modeId")}})
            result = {}
        elif method == "session/set_config_option":
            sid = params.get("sessionId")
            key = "mode" if params.get("configId") == "mode" else "model"
            sessions[sid][key] = params.get("value")
            result = {"configOptions": config_options(sessions[sid])}
        elif method == "session/cancel":
            cancelled.add(params.get("sessionId"))
            continue
        else:
            result = {"echoMethod": method, "echoParams": params}
        if rid is not None:
            send({"jsonrpc": "2.0", "id": rid, "result": result})
    except Exception as e:
        if rid is not None:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": str(e)}})
