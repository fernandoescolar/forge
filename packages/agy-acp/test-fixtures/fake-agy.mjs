#!/usr/bin/env node
// A stand-in for agy's stream-json print mode, as agy 1.3 behaves (see src/agy.ts).
//   "edit <file> <text>"  reads the file, then writes it with write_to_file, then answers
//   "run <cmd>"           runs a command with run_command, then answers
//   "slow"                streams for a long time (to cancel)
//   "weird"               sends a malformed step, then answers
//   anything else         answers "echo: <text>"
// Before each tool it runs the installed plugins' PreToolUse hooks, as agy does ("allow"
// or nothing printed goes on, anything else denies), then its own permission check: with
// --dangerously-skip-permissions everything runs; else commands are denied, and edits too
// unless --mode accept-edits (the turn then ends at once, with nothing said).
// Prints its arguments to stderr; `models` lists two models. SIGINT ends the turn
// ("interrupted") and the process, noted in FAKE_AGY_SIGNALS.
import { spawn } from "node:child_process";
import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
import { resolve } from "node:path";

const args = process.argv.slice(2);
// `agy mcp add|remove`, on the config file AGY_MCP_CONFIG names.
const configPath = process.env.AGY_MCP_CONFIG;
const readConfig = () => {
  try {
    const text = readFileSync(configPath, "utf8").trim();
    return text ? JSON.parse(text) : { mcpServers: {} };
  } catch {
    return { mcpServers: {} };
  }
};
if (args[0] === "mcp") {
  const config = readConfig();
  if (args[1] === "add") {
    let i = 2;
    const headers = {};
    while (args[i] === "--header" || args[i] === "--env") {
      if (args[i] === "--header") {
        const [k, ...v] = args[i + 1].split(": ");
        headers[k] = v.join(": ");
      }
      i += 2;
    }
    const name = args[i];
    const rest = args.slice(i + 1).filter((a) => a !== "--");
    config.mcpServers[name] = /^https?:/.test(rest[0]) ? { serverUrl: rest[0], headers } : { command: rest[0], args: rest.slice(1) };
  } else if (args[1] === "remove") {
    delete config.mcpServers[args[2]];
  }
  writeFileSync(configPath, JSON.stringify(config));
  process.exit(0);
}
// `agy plugin install <dir>|uninstall <name>`: the installed plugins' hooks, in AGY_PLUGINS_FILE.
const pluginsPath = process.env.AGY_PLUGINS_FILE;
const readPlugins = () => {
  try {
    return JSON.parse(readFileSync(pluginsPath, "utf8"));
  } catch {
    return {};
  }
};
if (args[0] === "plugin") {
  const plugins = readPlugins();
  if (process.env.FAKE_AGY_PLUGIN_FAIL) process.exit(1);
  if (args[1] === "install") {
    const name = JSON.parse(readFileSync(resolve(args[2], "plugin.json"), "utf8")).name;
    plugins[name] = JSON.parse(readFileSync(resolve(args[2], "hooks.json"), "utf8"));
  } else if (args[1] === "uninstall") {
    delete plugins[args[2]];
  }
  if (pluginsPath) writeFileSync(pluginsPath, JSON.stringify(plugins));
  process.exit(0);
}
if (args[0] === "models") {
  if (process.env.FAKE_AGY_UNAUTHENTICATED) {
    console.error("Error: authentication required. Run 'agy' to log in, then retry.");
    process.exit(1);
  }
  // Fails while the file FAKE_AGY_MODELS_FAIL names exists.
  if (process.env.FAKE_AGY_MODELS_FAIL && existsSync(process.env.FAKE_AGY_MODELS_FAIL)) {
    console.error("Error: couldn't reach the server");
    process.exit(1);
  }
  console.log("Fetching available models...\nfast-1\tFast One\nsmart-2\tSmart Two");
  process.exit(0);
}
console.error("ARGS " + JSON.stringify(args));
// What agy connects to when it starts: the servers configured right now.
if (process.env.FAKE_AGY_SEEN) appendFileSync(process.env.FAKE_AGY_SEEN, JSON.stringify(readConfig().mcpServers) + "\n");
// And the plugins it runs with.
if (process.env.FAKE_AGY_SEEN_HOOKS) appendFileSync(process.env.FAKE_AGY_SEEN_HOOKS, JSON.stringify(readPlugins()) + "\n");
if (process.env.FAKE_AGY_UNAUTHENTICATED) {
  console.error("Error: authentication required. Run 'agy' to log in, then retry.");
  console.log(JSON.stringify({ event: "result", result: { conversation_id: "", status: "ERROR", error: "authentication failed or timed out" } }));
  process.exit(1);
}
const at = args.indexOf("--conversation");
const id = at >= 0 ? args[at + 1] : "conv-" + process.pid;
const out = (o) => console.log(JSON.stringify(o));
const step = (s) => out({ event: "step_update", step_update: { conversation_id: id, ...s } });
let index = 0;
const skip = args.includes("--dangerously-skip-permissions");
const modeAt = args.indexOf("--mode");
const mode = modeAt >= 0 ? args[modeAt + 1] : "default";

process.on("SIGINT", () => {
  if (process.env.FAKE_AGY_SIGNALS) appendFileSync(process.env.FAKE_AGY_SIGNALS, process.env.FAKE_AGY_IGNORE_SIGINT ? "SIGINT ignored\n" : "SIGINT\n");
  if (process.env.FAKE_AGY_IGNORE_SIGINT) return;
  console.error("error: interrupted");
  out({ event: "result", result: { conversation_id: id, status: "ERROR", error: "interrupted" } });
  process.exit(1);
});

out({ event: "init", conversation_id: id, init: { cwd: process.cwd(), tools: [], permission_mode: "request-review" } });

/** The installed plugins' PreToolUse hook commands for a tool. */
function hooksFor(name) {
  const commands = [];
  for (const file of Object.values(readPlugins())) {
    for (const set of Object.values(file ?? {})) {
      for (const group of set?.PreToolUse ?? []) {
        if (group.matcher === "*" || group.matcher === "" || new RegExp(`^(${group.matcher})$`).test(name)) commands.push(...group.hooks.map((h) => h.command));
      }
    }
  }
  return commands;
}

function runHook(command, input) {
  return new Promise((done) => {
    const child = spawn(command, { shell: true, stdio: ["pipe", "pipe", "inherit"] });
    let output = "";
    child.stdout.on("data", (d) => (output += d));
    child.on("close", () => done(output));
    child.stdin.end(input);
  });
}

/** Why the hooks deny a tool, if they do. */
async function hookDenial(i, info) {
  for (const command of hooksFor(info.name)) {
    const output = await runHook(command, JSON.stringify({ toolCall: { name: info.name, args: info.parameters }, stepIdx: i, conversationId: id }));
    if (process.env.FAKE_AGY_SEEN_HOOK_ANSWERS) appendFileSync(process.env.FAKE_AGY_SEEN_HOOK_ANSWERS, JSON.stringify({ tool: info.name, output: output.trim() }) + "\n");
    if (!output.trim()) continue;
    let answer = {};
    try {
      answer = JSON.parse(output);
    } catch {}
    if (answer.decision === "allow") continue;
    return `tool call denied by pre-tool hook: ${answer.reason ?? ""}`;
  }
  return undefined;
}

/** What agy's own check denies (print mode can't ask). */
function permissionDenial(name) {
  if (skip) return undefined;
  if (name === "run_command") return { message: "user denied permission to run command", action: "command", display_name: "RunCommand" };
  if (name === "write_to_file" && mode !== "accept-edits") return { message: "user denied permission for write_file", action: "write_file", display_name: "WriteToFile" };
  return undefined;
}

/** Runs a tool as agy does; "ran", "hook" (denied by a hook) or "denied". */
async function tool(info, run) {
  const i = index++;
  step({ step_index: i, state: "ACTIVE", step_type: "tool", tool_name: info.name, tool_info: info });
  const fail = (message) => step({ step_index: i, state: "ERROR", step_type: "tool", tool_name: info.name, tool_info: { ...info, error: { type: "TOOL_ERROR", message } } });
  const hooked = await hookDenial(i, info);
  if (hooked) {
    fail(hooked);
    return "hook";
  }
  const denied = permissionDenial(info.name);
  if (denied) {
    fail(denied.message);
    // As agy does: the turn ends at once, with nothing said.
    out({ event: "result", result: { conversation_id: id, status: "SUCCESS", response: "", usage: {}, denied_actions: [{ action: denied.action, display_name: denied.display_name }] } });
    return "denied";
  }
  const output = run();
  step({ step_index: i, state: "DONE", step_type: "tool", tool_name: info.name, tool_info: { ...info, ...(output ? { output } : {}) } });
  return "ran";
}

// One message at a time, as agy takes them.
let turns = Promise.resolve();
createInterface({ input: process.stdin }).on("line", (line) => {
  turns = turns.then(() => turn(line));
});

async function turn(line) {
  const msg = JSON.parse(line);
  if (msg.event !== "user") return;
  const text = msg.message.content;
  step({ step_index: index++, state: "DONE", step_type: "user_input" });
  const words = text.split(" ");
  if (words[0] === "edit") {
    const file = resolve(process.cwd(), words[1]);
    // As agy does: read the file, think (a model step), then edit it, writing it right
    // after announcing the edit.
    await tool({ name: "view_file", parameters: { AbsolutePath: file } }, () => "1 lines");
    await new Promise((r) => setTimeout(r, 300));
    const content = words.slice(2).join(" ") + "\n";
    const ran = await tool({ name: "write_to_file", parameters: { TargetFile: file, CodeContent: content } }, () => writeFileSync(file, content));
    if (ran !== "denied") answer(text);
  } else if (words[0] === "run") {
    const ran = await tool({ name: "run_command", parameters: { CommandLine: words.slice(1).join(" "), Cwd: process.cwd() } }, () => `ran: ${words.slice(1).join(" ")}`);
    if (ran !== "denied") answer(text);
  } else if (words[0] === "weird") {
    // A step the adapter can't make sense of (its text isn't text).
    step({ step_index: index++, state: "DONE", step_type: "agent_response", text_delta: 5 });
    answer(text);
  } else if (words[0] === "slow") {
    const i = index++;
    let n = 0;
    setInterval(() => step({ step_index: i, state: "ACTIVE", step_type: "agent_response", text_delta: `${n++} ` }), 50);
    await new Promise(() => {});
  } else {
    answer(text);
  }
}

function answer(text) {
  const i = index++;
  step({ step_index: i, state: "ACTIVE", step_type: "agent_response", text_delta: "echo: " });
  step({ step_index: i, state: "DONE", step_type: "agent_response", text_delta: text, usage: { input_tokens: 10, output_tokens: 2, total_tokens: 12 } });
  out({ event: "result", result: { conversation_id: id, status: "SUCCESS", usage: { input_tokens: 10, output_tokens: 2, total_tokens: 12 } } });
}
