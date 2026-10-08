import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";
import { agyArgs, isAuthError, userMessage } from "../agy.js";
import { parseModels, promptText } from "../agent.js";
import { toolKind, toolPaths, toolTitle } from "../tools.js";
import { addArgs } from "../mcp.js";

const root = resolve(import.meta.dirname, "../..");
const fakeAgy = join(root, "test-fixtures/fake-agy.mjs");

test("agy's command line follows the session's settings", () => {
  const base = ["--input-format", "stream-json", "--output-format", "stream-json"];
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "editsOnly" }), [...base, "--mode", "accept-edits"]);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "bypassPermissions", model: "m", effort: "high", conversationId: "c1", additionalDirectories: ["/x"] }), [
    ...base, "--dangerously-skip-permissions", "--model", "m", "--effort", "high", "--conversation", "c1", "--add-dir", "/x",
  ]);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "plan" }), [...base, "--mode", "plan"]);
  assert.equal(userMessage("hi"), '{"event":"user","message":{"role":"user","content":"hi"}}');
});

test("tools become ACP tool calls", () => {
  const edit = { name: "replace_file_content", parameters: { TargetFile: "/p/src/a.rs", Instruction: "x" } };
  assert.equal(toolKind("replace_file_content"), "edit");
  assert.deepEqual(toolPaths(edit), ["/p/src/a.rs"]);
  assert.equal(toolTitle(edit.name, edit, "/p"), "Edit src/a.rs");
  assert.equal(toolTitle("view_file", { name: "view_file", parameters: { AbsolutePath: "/elsewhere/b.rs" } }, "/p"), "Read /elsewhere/b.rs");
  assert.equal(toolTitle("run_command", { name: "run_command", parameters: { CommandLine: "cargo test" } }, "/p"), "Run `cargo test`");
  assert.equal(toolTitle("grep_search", { name: "grep_search", parameters: { Query: "fn main", SearchPath: "/p/src" } }, "/p"), "Search `fn main` in src");
  assert.equal(toolTitle("call_mcp_tool", { name: "call_mcp_tool", parameters: { ServerName: "forge", ToolName: "run_tests", Arguments: {} } }, "/p"), "run_tests (forge)");
  assert.equal(toolTitle("view_file", { name: "view_file", parameters: { AbsolutePath: "/Users/me/.gemini/antigravity-cli/mcp/forge/run_tests.json" } }, "/p"), "Look up the run_tests tool (forge)");
  assert.equal(toolKind("browser_click_element"), "other");
  assert.equal(toolTitle("browser_click_element", undefined, "/p"), "Browser click element");
});

test("MCP servers become agy mcp add arguments", () => {
  assert.deepEqual(addArgs({ type: "http", name: "forge", url: "http://127.0.0.1:1/mcp/t", headers: [{ name: "Authorization", value: "Bearer x" }] }), ["mcp", "add", "--header", "Authorization: Bearer x", "forge", "http://127.0.0.1:1/mcp/t"]);
  assert.deepEqual(addArgs({ name: "fs", command: "npx", args: ["-y", "server"], env: [{ name: "K", value: "v" }] }), ["mcp", "add", "--env", "K=v", "fs", "--", "npx", "-y", "server"]);
});

test("prompts, models and errors", () => {
  assert.equal(
    promptText([
      { type: "text", text: "Fix this" },
      { type: "resource", resource: { uri: "file:///p/a.rs", text: "fn a() {}" } },
      { type: "resource_link", uri: "file:///p/b.rs", name: "b.rs" },
    ]),
    "Fix this\n\nfile:///p/a.rs:\n```\nfn a() {}\n```\n\n@file:///p/b.rs",
  );
  assert.deepEqual(parseModels("Fetching available models...\ngemini-3-flash\tGemini 3 Flash\n"), [{ value: "gemini-3-flash", name: "Gemini 3 Flash" }]);
  assert.ok(isAuthError("Error: authentication required. Run 'agy' to log in, then retry."));
  assert.ok(!isAuthError("rate limited"));
});

/** The adapter as a client runs it, against the fake agy (with its own MCP config and notes). */
function connect(env: Record<string, string> = {}) {
  const home = mkdtempSync(join(tmpdir(), "agy-acp-home-"));
  const isolated = { AGY_MCP_CONFIG: join(home, "mcp_config.json"), XDG_CONFIG_HOME: join(home, "config") };
  const child = spawn(process.execPath, [join(root, "dist/index.js")], { env: { ...process.env, AGY_BIN: fakeAgy, ...isolated, ...env }, stdio: ["pipe", "pipe", "inherit"] });
  const updates: acp.SessionNotification[] = [];
  const client: acp.Client = {
    async sessionUpdate(params) {
      updates.push(params);
    },
    async requestPermission() {
      return { outcome: { outcome: "cancelled" } };
    },
  };
  const stream = acp.ndJsonStream(Writable.toWeb(child.stdin), Readable.toWeb(child.stdout) as ReadableStream<Uint8Array>);
  const connection = new acp.ClientSideConnection(() => client, stream);
  return { connection, updates, close: () => child.kill() };
}

test("a session answers, edits files with whole-file diffs, and keeps its conversation", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  writeFileSync(join(cwd, "a.txt"), "old\n");
  const { connection, updates, close } = connect();
  try {
    const init = await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    assert.equal(init.agentCapabilities?.loadSession, true);
    assert.deepEqual(init.authMethods?.map((m) => m.id), ["gemini-api-key", "google-login"]);

    const session = await connection.newSession({ cwd, mcpServers: [] });
    assert.match(session.sessionId, /^conv-/);
    assert.equal(session.modes?.currentModeId, "bypassPermissions");
    assert.deepEqual(session.configOptions?.find((o) => o.id === "model")?.type === "select" && (session.configOptions.find((o) => o.id === "model") as { options: { value: string }[] }).options.map((o) => o.value), ["default", "fast-1", "smart-2"]);

    const answer = await connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "hello" }] });
    assert.equal(answer.stopReason, "end_turn");
    assert.equal(answer.usage?.totalTokens, 12);
    const text = updates.flatMap((u) => (u.update.sessionUpdate === "agent_message_chunk" && u.update.content.type === "text" ? [u.update.content.text] : [])).join("");
    assert.equal(text, "echo: hello");

    updates.length = 0;
    await connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "edit a.txt new" }] });
    assert.equal(readFileSync(join(cwd, "a.txt"), "utf8"), "new\n");
    const call = updates.filter((u) => u.update.sessionUpdate === "tool_call").map((u) => u.update as acp.ToolCall & { sessionUpdate: string }).find((c) => c.kind === "edit")!;
    assert.equal(call.kind, "edit");
    assert.equal(call.title, "Edit a.txt");
    const done = updates.map((u) => u.update).find((u) => u.sessionUpdate === "tool_call_update" && u.toolCallId === call.toolCallId) as acp.ToolCallUpdate;
    assert.equal(done.status, "completed");
    // agy wrote the file right after announcing the edit: the diff is against what it read.
    assert.deepEqual(done.content?.[0], { type: "diff", path: join(realpathSync(cwd), "a.txt"), oldText: "old\n", newText: "new\n" });

    // In the Edit files mode, a command the print mode can't ask for fails, with agy's reason.
    await connection.setSessionMode({ sessionId: session.sessionId, modeId: "editsOnly" });
    updates.length = 0;
    await connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "run cargo test" }] });
    const denied = updates.find((u) => u.update.sessionUpdate === "tool_call_update")?.update as acp.ToolCallUpdate;
    assert.equal(denied.status, "failed");
    assert.match(JSON.stringify(denied.content), /denied permission/);
    // agy ends the turn without a word; the adapter says why, and what to do.
    const said = updates.flatMap((u) => (u.update.sessionUpdate === "agent_message_chunk" && u.update.content.type === "text" ? [u.update.content.text] : [])).join("");
    assert.match(said, /needed to run `cargo test`.*Edit files.*Full access/);

    // Cancelling stops agy; the next prompt continues the same conversation.
    const slow = connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "slow" }] });
    await new Promise((r) => setTimeout(r, 300));
    await connection.cancel({ sessionId: session.sessionId });
    assert.equal((await slow).stopReason, "cancelled");
    updates.length = 0;
    await connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "again" }] });
    assert.ok(updates.some((u) => u.sessionId === session.sessionId));

    // A new mode applies from the next prompt.
    await connection.setSessionMode({ sessionId: session.sessionId, modeId: "bypassPermissions" });
    await connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "after" }] });
  } finally {
    close();
  }
});

test("a session resumes by its id", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const { connection, updates, close } = connect();
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    await connection.loadSession({ sessionId: "conv-old", cwd, mcpServers: [] });
    await connection.prompt({ sessionId: "conv-old", prompt: [{ type: "text", text: "back" }] });
    assert.ok(updates.every((u) => u.sessionId === "conv-old"));
  } finally {
    close();
  }
});

test("the session's MCP servers are in agy's config while its agy runs", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const home = mkdtempSync(join(tmpdir(), "agy-acp-mcp-"));
  const config = join(home, "mcp_config.json");
  const seen = join(home, "seen.jsonl");
  const notes = join(home, "config");
  const note = join(notes, "forge-agy-acp", "added-mcp-servers.json");
  const servers = () => Object.keys(JSON.parse(readFileSync(config, "utf8")).mcpServers).sort();
  // The user's own server, and one an adapter that died (pid 999999) left behind.
  writeFileSync(config, JSON.stringify({ mcpServers: { mine: { serverUrl: "http://user/mcp" }, stale: { serverUrl: "http://old/mcp" } } }));
  mkdirSync(join(notes, "forge-agy-acp"), { recursive: true });
  writeFileSync(note, JSON.stringify({ stale: 999999 }));
  const { connection, close } = connect({ AGY_MCP_CONFIG: config, XDG_CONFIG_HOME: notes, FAKE_AGY_SEEN: seen });
  try {
    const init = await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    assert.equal(init.agentCapabilities?.mcpCapabilities?.http, true);
    const forge = { type: "http" as const, name: "forge", url: "http://127.0.0.1:1/mcp/token", headers: [] };
    const clash = { type: "http" as const, name: "mine", url: "http://forge/mcp/other", headers: [] };
    const session = await connection.newSession({ cwd, mcpServers: [forge, clash] });
    const atStart = JSON.parse(readFileSync(seen, "utf8").trim().split("\n")[0]);
    assert.deepEqual(Object.keys(atStart).sort(), ["forge", "mine"], "agy started with the session's server and the user's, not the dead adapter's leftover");
    assert.equal(atStart.forge.serverUrl, forge.url);
    assert.equal(atStart.mine.serverUrl, "http://user/mcp", "the user's server of the same name is left alone");
    assert.deepEqual(servers(), ["forge", "mine"], "the session's server stays while its agy runs");

    // A second session shares it; it goes once neither runs.
    const other = await connection.newSession({ cwd, mcpServers: [forge] });
    await connection.closeSession?.({ sessionId: other.sessionId }).catch(() => {});
    await connection.cancel({ sessionId: session.sessionId });
    await connection.prompt({ sessionId: session.sessionId, prompt: [{ type: "text", text: "still there" }] });
    assert.deepEqual(servers(), ["forge", "mine"]);
  } finally {
    close();
  }
  // The adapter closed: what it added is gone, the user's stays.
  await new Promise((r) => setTimeout(r, 500));
  assert.deepEqual(servers(), ["mine"]);
  assert.equal(existsSync(note), false);
});

test("signing in is asked for when agy isn't", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const { connection, close } = connect({ FAKE_AGY_UNAUTHENTICATED: "1" });
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    await assert.rejects(connection.newSession({ cwd, mcpServers: [] }), (e: { code?: number; message?: string }) => e.code === -32000 && /Authentication required/.test(e.message ?? ""));
  } finally {
    close();
  }
});
