import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";
import { agyArgs, isAuthError, userMessage } from "../agy.js";
import { parseModels, promptText, saveImages } from "../agent.js";
import { hookCommand, hooksJson, pluginName } from "../hooks.js";
import { alwaysKey, asks, hookAnswer, proposedDiff } from "../permissions.js";
import { toolKind, toolPaths, toolTitle } from "../tools.js";
import { addArgs } from "../mcp.js";

const root = resolve(import.meta.dirname, "../..");
const fakeAgy = join(root, "test-fixtures/fake-agy.mjs");
const guard = `"${process.execPath}" "${join(root, "test-fixtures/guard.mjs")}"`;

test("agy's command line follows the session's settings", () => {
  const base = ["--input-format", "stream-json", "--output-format", "stream-json"];
  const skip = "--dangerously-skip-permissions";
  // With the adapter's hook, it decides; without, agy's own checks (which only deny).
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "default" }), base);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "default", hooked: true }), [...base, skip]);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "acceptEdits" }), [...base, "--mode", "accept-edits"]);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "acceptEdits", hooked: true }), [...base, skip]);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "bypassPermissions", model: "m", effort: "high", conversationId: "c1", additionalDirectories: ["/x"] }), [
    ...base, skip, "--model", "m", "--effort", "high", "--conversation", "c1", "--add-dir", "/x",
  ]);
  assert.deepEqual(agyArgs({ cwd: "/p", mode: "plan", hooked: true }), [...base, "--mode", "plan"]);
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

test("which tools ask the client, by mode", () => {
  const roots = ["/p"];
  const run = { CommandLine: "cargo test" };
  const inProject = { TargetFile: "/p/a.rs" };
  const outside = { TargetFile: "/etc/hosts" };
  assert.ok(asks("default", "run_command", run, roots));
  assert.ok(asks("default", "write_to_file", inProject, roots));
  assert.ok(asks("default", "call_mcp_tool", {}, roots));
  assert.ok(!asks("default", "view_file", { AbsolutePath: "/etc/hosts" }, roots), "reads don't ask");
  assert.ok(!asks("default", "ask_question", {}, roots));
  assert.ok(!asks("acceptEdits", "write_to_file", inProject, roots), "edits in the project go ahead");
  assert.ok(asks("acceptEdits", "write_to_file", outside, roots), "edits elsewhere ask");
  assert.ok(asks("acceptEdits", "run_command", run, roots));
  assert.ok(!asks("bypassPermissions", "run_command", run, roots));
  assert.ok(!asks("plan", "write_to_file", inProject, roots));
  assert.equal(alwaysKey("run_command", run), "run_command:cargo test");
  assert.equal(alwaysKey("write_to_file", inProject), "write_to_file");
});

test("an edit's diff is shown before it is made", async () => {
  const dir = mkdtempSync(join(tmpdir(), "agy-acp-diff-"));
  const file = join(dir, "a.txt");
  writeFileSync(file, "one\ntwo\ntwo\n");
  assert.deepEqual(await proposedDiff("write_to_file", { TargetFile: file, CodeContent: "new\n" }), [{ type: "diff", path: file, oldText: "one\ntwo\ntwo\n", newText: "new\n" }]);
  assert.deepEqual(await proposedDiff("write_to_file", { TargetFile: join(dir, "b.txt"), CodeContent: "b\n" }), [{ type: "diff", path: join(dir, "b.txt"), oldText: null, newText: "b\n" }]);
  const single = await proposedDiff("replace_file_content", { TargetFile: file, TargetContent: "two", ReplacementContent: "2" });
  assert.equal((single[0] as { newText: string }).newText, "one\n2\ntwo\n");
  const multi = await proposedDiff("multi_replace_file_content", { TargetFile: file, ReplacementChunks: [{ TargetContent: "one", ReplacementContent: "1" }, { TargetContent: "two", ReplacementContent: "2", AllowMultiple: true }] });
  assert.equal((multi[0] as { newText: string }).newText, "1\n2\n2\n");
  assert.deepEqual(await proposedDiff("replace_file_content", { TargetFile: file, TargetContent: "missing", ReplacementContent: "x" }), [], "no guess when the target isn't there");
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

test("images are saved as files agy looks at", () => {
  const dir = mkdtempSync(join(tmpdir(), "agy-acp-img-"));
  const blocks: acp.ContentBlock[] = [
    { type: "text", text: "What's this?" },
    { type: "image", mimeType: "image/png", data: Buffer.from("PNGDATA").toString("base64") },
  ];
  const [path] = saveImages(blocks, dir);
  assert.match(path, /image-\d+\.png$/);
  assert.equal(readFileSync(path, "utf8"), "PNGDATA");
  assert.equal(promptText(blocks, [path]), `What's this?\n\n[Attached image: ${path} (look at it with view_file)]`);
});

test("the hook's plugin is per install, and only acts for its adapter's agy", async () => {
  assert.notEqual(pluginName("/usr/bin/node", "/a/dist/hook.js"), pluginName("/usr/bin/node", "/b/dist/hook.js"));
  assert.match(pluginName(), /^forge-guard-[0-9a-f]{10}$/);
  const name = pluginName();
  assert.deepEqual(hooksJson(name, hookCommand(name, "/n", "/h.js")), { [name]: { PreToolUse: [{ matcher: "*", hooks: [{ type: "command", command: `"/n" "/h.js" ${name}`, timeout: 3600 }] }] } });
  const input = JSON.stringify({ toolCall: { name: "run_command", args: { CommandLine: "cat a.rs" } } });
  // The user's own agy, or another install's: no opinion.
  assert.equal(await hookAnswer(input, name, {}), "");
  assert.equal(await hookAnswer(input, name, { FORGE_AGY_ACP_HOOK: "forge-guard-other", FORGE_AGY_ACP_SHELL_HOOK: guard }), "");
  // Its own: the client's shell hook, then the adapter (which isn't there: denied).
  assert.match(await hookAnswer(input, name, { FORGE_AGY_ACP_HOOK: name, FORGE_AGY_ACP_SHELL_HOOK: guard }), /"deny".*read_file/);
  const ls = JSON.stringify({ toolCall: { name: "run_command", args: { CommandLine: "ls" } } });
  assert.equal(await hookAnswer(ls, name, { FORGE_AGY_ACP_HOOK: name, FORGE_AGY_ACP_SHELL_HOOK: guard }), "");
  assert.match(await hookAnswer(ls, name, { FORGE_AGY_ACP_HOOK: name, FORGE_AGY_ACP_SOCKET: join(tmpdir(), "nobody-here.sock") }), /"deny".*didn't answer/);
});

type Answer = (request: acp.RequestPermissionRequest) => string | undefined;

/** The adapter as a client runs it, against the fake agy (with its own MCP config and notes). */
function connect(env: Record<string, string> = {}, answer: Answer = () => "allow") {
  const home = mkdtempSync(join(tmpdir(), "agy-acp-home-"));
  const isolated = { AGY_MCP_CONFIG: join(home, "mcp_config.json"), AGY_PLUGINS_FILE: join(home, "plugins.json"), XDG_CONFIG_HOME: join(home, "config") };
  const child = spawn(process.execPath, [join(root, "dist/index.js")], { env: { ...process.env, AGY_BIN: fakeAgy, ...isolated, ...env }, stdio: ["pipe", "pipe", "pipe"] });
  const updates: acp.SessionNotification[] = [];
  const asked: acp.RequestPermissionRequest[] = [];
  let stderr = "";
  child.stderr.on("data", (d) => (stderr += d));
  const client: acp.Client = {
    async sessionUpdate(params) {
      updates.push(params);
    },
    async requestPermission(params) {
      asked.push(params);
      const option = answer(params);
      return option ? { outcome: { outcome: "selected", optionId: option } } : { outcome: { outcome: "cancelled" } };
    },
  };
  const stream = acp.ndJsonStream(Writable.toWeb(child.stdin), Readable.toWeb(child.stdout) as ReadableStream<Uint8Array>);
  const connection = new acp.ClientSideConnection(() => client, stream);
  return { connection, updates, asked, stderr: () => stderr, close: () => child.kill() };
}

const said = (updates: acp.SessionNotification[]) =>
  updates.flatMap((u) => (u.update.sessionUpdate === "agent_message_chunk" && u.update.content.type === "text" ? [u.update.content.text] : [])).join("");

const toolUpdates = (updates: acp.SessionNotification[]) => updates.map((u) => u.update).filter((u) => u.sessionUpdate === "tool_call_update") as acp.ToolCallUpdate[];

test("a session asks the client before edits and commands, by its mode", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  writeFileSync(join(cwd, "a.txt"), "old\n");
  const signals = join(cwd, "signals.txt");
  let reply = "allow";
  const { connection, updates, asked, close } = connect({ FAKE_AGY_SIGNALS: signals }, () => reply);
  try {
    const init = await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    assert.equal(init.agentCapabilities?.loadSession, true);
    assert.equal(init.agentCapabilities?.promptCapabilities?.image, true);
    assert.deepEqual(init.authMethods?.map((m) => m.id), ["gemini-api-key", "google-login"]);

    const session = await connection.newSession({ cwd, mcpServers: [] });
    assert.match(session.sessionId, /^conv-/);
    assert.equal(session.modes?.currentModeId, "default");
    assert.deepEqual(session.modes?.availableModes.map((m) => m.id), ["default", "acceptEdits", "bypassPermissions", "plan"]);
    const model = session.configOptions?.find((o) => o.id === "model") as { options: { value: string }[] } | undefined;
    assert.deepEqual(model?.options.map((o) => o.value), ["default", "fast-1", "smart-2"]);
    const sessionId = session.sessionId;

    const answer = await connection.prompt({ sessionId, prompt: [{ type: "text", text: "hello" }] });
    assert.equal(answer.stopReason, "end_turn");
    assert.equal(answer.usage?.totalTokens, 12);
    assert.equal(said(updates), "echo: hello");
    assert.equal(asked.length, 0);

    // An edit asks, showing the diff it will make; allowed, it is made.
    updates.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "edit a.txt new" }] });
    assert.equal(asked.length, 1, "the read didn't ask, the edit did");
    assert.equal(asked[0].toolCall.kind, "edit");
    assert.equal(asked[0].toolCall.title, "Edit a.txt");
    assert.deepEqual(asked[0].toolCall.content?.[0], { type: "diff", path: join(realpathSync(cwd), "a.txt"), oldText: "old\n", newText: "new\n" });
    assert.deepEqual(asked[0].options.map((o) => o.kind), ["allow_once", "allow_always", "reject_once"]);
    assert.equal(readFileSync(join(cwd, "a.txt"), "utf8"), "new\n");
    const call = updates.filter((u) => u.update.sessionUpdate === "tool_call").map((u) => u.update as acp.ToolCall & { sessionUpdate: string }).find((c) => c.kind === "edit")!;
    assert.equal(asked[0].toolCall.toolCallId, call.toolCallId, "the request is about the tool call shown");
    const done = toolUpdates(updates).find((u) => u.toolCallId === call.toolCallId)!;
    assert.equal(done.status, "completed");
    assert.deepEqual(done.content?.[0], { type: "diff", path: join(realpathSync(cwd), "a.txt"), oldText: "old\n", newText: "new\n" });

    // Rejected, it doesn't run, and agy tells the model why.
    reply = "reject";
    asked.length = 0;
    updates.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "run cargo test" }] });
    assert.equal(asked[0].toolCall.title, "Run `cargo test`");
    assert.equal((asked[0].toolCall.rawInput as { command: string }).command, "cargo test");
    const rejected = toolUpdates(updates)[0];
    assert.equal(rejected.status, "failed");
    assert.match(JSON.stringify(rejected.content), /denied by pre-tool hook: The user rejected/);

    // Always allowed: not asked again in the session.
    reply = "always";
    asked.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "run ls" }] });
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "run ls" }] });
    assert.equal(asked.length, 1);

    // Edit files: edits in the project go ahead, commands still ask.
    reply = "allow";
    await connection.setSessionMode({ sessionId, modeId: "acceptEdits" });
    asked.length = 0;
    updates.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "edit a.txt newer" }] });
    assert.equal(asked.length, 0);
    assert.equal(readFileSync(join(cwd, "a.txt"), "utf8"), "newer\n");
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "run make" }] });
    assert.equal(asked.length, 1);

    // Full access: nothing asks.
    await connection.setSessionMode({ sessionId, modeId: "bypassPermissions" });
    asked.length = 0;
    updates.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "run make" }] });
    assert.equal(asked.length, 0);
    assert.equal(toolUpdates(updates)[0].status, "completed");

    // Cancelling interrupts agy (SIGINT); the next prompt continues the same conversation.
    const slow = connection.prompt({ sessionId, prompt: [{ type: "text", text: "slow" }] });
    await new Promise((r) => setTimeout(r, 300));
    await connection.cancel({ sessionId });
    assert.equal((await slow).stopReason, "cancelled");
    updates.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "again" }] });
    assert.equal(said(updates), "echo: again");
    assert.equal(readFileSync(signals, "utf8"), "SIGINT\n");
  } finally {
    close();
  }
});

test("a cancelled agy that ignores SIGINT is killed", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const signals = join(cwd, "signals.txt");
  const { connection, updates, close } = connect({ FAKE_AGY_SIGNALS: signals, FAKE_AGY_IGNORE_SIGINT: "1" });
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    const { sessionId } = await connection.newSession({ cwd, mcpServers: [] });
    const slow = connection.prompt({ sessionId, prompt: [{ type: "text", text: "slow" }] });
    await new Promise((r) => setTimeout(r, 300));
    await connection.cancel({ sessionId });
    assert.equal((await slow).stopReason, "cancelled");
    updates.length = 0;
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "again" }] });
    assert.equal(said(updates), "echo: again");
    assert.equal(readFileSync(signals, "utf8"), "SIGINT ignored\n");
  } finally {
    close();
  }
});

test("without the hook, agy's print mode denies, and the adapter says why", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const { connection, updates, asked, close } = connect({ FAKE_AGY_PLUGIN_FAIL: "1" });
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    const { sessionId } = await connection.newSession({ cwd, mcpServers: [] });
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "run cargo test" }] });
    assert.equal(asked.length, 0);
    const denied = toolUpdates(updates)[0];
    assert.equal(denied.status, "failed");
    assert.match(JSON.stringify(denied.content), /denied permission/);
    assert.match(said(updates), /needed to run `cargo test`.*permission hook.*Full access/);
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

test("an image in a prompt reaches agy as a file", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const { connection, updates, close } = connect();
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    const { sessionId } = await connection.newSession({ cwd, mcpServers: [] });
    const data = Buffer.from("not really a png").toString("base64");
    await connection.prompt({ sessionId, prompt: [{ type: "text", text: "look" }, { type: "image", mimeType: "image/png", data }] });
    const path = /\[Attached image: (\S+)/.exec(said(updates))?.[1];
    assert.ok(path, said(updates));
    assert.equal(readFileSync(path, "utf8"), "not really a png");
  } finally {
    close();
  }
});

test("the models are listed again after a failed listing", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const failing = join(cwd, "models-fail");
  writeFileSync(failing, "");
  const { connection, close } = connect({ FAKE_AGY_MODELS_FAIL: failing });
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    const first = await connection.newSession({ cwd, mcpServers: [] });
    assert.equal(first.configOptions?.some((o) => o.id === "model"), false);
    rmSync(failing);
    const second = await connection.newSession({ cwd, mcpServers: [] });
    assert.equal(second.configOptions?.some((o) => o.id === "model"), true);
  } finally {
    close();
  }
});

test("an event the adapter can't handle is logged, and the turn goes on", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const { connection, updates, stderr, close } = connect();
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    const { sessionId } = await connection.newSession({ cwd, mcpServers: [] });
    const answer = await connection.prompt({ sessionId, prompt: [{ type: "text", text: "weird" }] });
    assert.equal(answer.stopReason, "end_turn");
    assert.equal(said(updates), "echo: weird");
    assert.match(stderr(), /couldn't pass on agy's step_update event/);
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

test("the hook is an agy plugin of this install while sessions use it", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const home = mkdtempSync(join(tmpdir(), "agy-acp-hooks-"));
  const plugins = join(home, "plugins.json");
  const seen = join(home, "seen-hooks.jsonl");
  const answers = join(home, "hook-answers.jsonl");
  const notes = join(home, "config");
  const ours = pluginName(process.execPath, join(root, "dist/hook.js"));
  // An older adapter that died (pid 999999) left its plugin installed; another install's
  // adapter (alive: this test) uses its own.
  writeFileSync(plugins, JSON.stringify({ "forge-guard": { stale: true }, "forge-guard-other": { other: true }, mine: { PostToolUse: [] } }));
  mkdirSync(join(notes, "forge-agy-acp"), { recursive: true });
  writeFileSync(join(notes, "forge-agy-acp", "guard-users.json"), JSON.stringify([999999]));
  writeFileSync(join(notes, "forge-agy-acp", "hook-users.json"), JSON.stringify({ "forge-guard-other": [process.pid] }));
  const env = { AGY_PLUGINS_FILE: plugins, XDG_CONFIG_HOME: notes, FAKE_AGY_SEEN_HOOKS: seen, FAKE_AGY_SEEN_HOOK_ANSWERS: answers };
  const meta = { forge: { shellHook: { command: guard } } };
  const first = connect(env);
  const second = connect(env);
  try {
    await first.connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    await second.connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    // Full access: the hook only guards shell commands.
    const { sessionId } = await first.connection.newSession({ cwd, mcpServers: [], _meta: meta });
    await first.connection.setSessionMode({ sessionId, modeId: "bypassPermissions" });
    await second.connection.newSession({ cwd, mcpServers: [] });
    const atStart = JSON.parse(readFileSync(seen, "utf8").trim().split("\n")[0]);
    assert.deepEqual(atStart[ours], hooksJson(ours, hookCommand(ours, process.execPath, join(root, "dist/hook.js"))));
    assert.equal(atStart["forge-guard"], undefined, "the dead older adapter's plugin is gone");
    assert.deepEqual(atStart["forge-guard-other"], { other: true }, "another install's plugin stays");
    assert.deepEqual(atStart.mine, { PostToolUse: [] }, "the user's plugins stay");

    first.updates.length = 0;
    await first.connection.prompt({ sessionId, prompt: [{ type: "text", text: "run cat a.rs" }] });
    assert.match(JSON.stringify(toolUpdates(first.updates)[0].content), /denied by pre-tool hook: Read files with read_file/);
    assert.equal(first.asked.length, 0);
    first.updates.length = 0;
    await first.connection.prompt({ sessionId, prompt: [{ type: "text", text: "run ls" }] });
    assert.equal(toolUpdates(first.updates)[0].status, "completed");
  } finally {
    first.close();
  }
  await new Promise((r) => setTimeout(r, 500));
  assert.ok(ours in JSON.parse(readFileSync(plugins, "utf8")), "still installed while the other adapter uses it");
  second.close();
  await new Promise((r) => setTimeout(r, 500));
  assert.deepEqual(Object.keys(JSON.parse(readFileSync(plugins, "utf8"))).sort(), ["forge-guard-other", "mine"], "uninstalled when the last adapter ends");
});

test("signing in is asked for when agy isn't, with what to run", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "agy-acp-"));
  const { connection, close } = connect({ FAKE_AGY_UNAUTHENTICATED: "1" });
  try {
    await connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    await assert.rejects(connection.newSession({ cwd, mcpServers: [] }), (e: { code?: number; message?: string }) => e.code === -32000 && /Authentication required/.test(e.message ?? ""));
    await assert.rejects(connection.authenticate({ methodId: "google-login" }), (e: { code?: number; message?: string }) => e.code === -32000 && /--login/.test(e.message ?? ""));
    await assert.rejects(connection.authenticate({ methodId: "nope" }));
  } finally {
    close();
  }
  const signedIn = connect({ GEMINI_API_KEY: "key" });
  try {
    await signedIn.connection.initialize({ protocolVersion: acp.PROTOCOL_VERSION, clientCapabilities: {} });
    await signedIn.connection.authenticate({ methodId: "gemini-api-key" });
    await signedIn.connection.authenticate({ methodId: "google-login" });
  } finally {
    signedIn.close();
  }
});
