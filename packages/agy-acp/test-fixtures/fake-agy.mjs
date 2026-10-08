#!/usr/bin/env node
// A stand-in for agy's stream-json print mode, as agy 1.3 behaves (see src/agy.ts).
//   "edit <file> <text>"  edits the file with replace_file_content, then answers
//   "run <cmd>"           a run_command the print mode denies
//   "slow"                streams for a long time (to cancel)
//   anything else         answers "echo: <text>"
// Prints its arguments to stderr; `models` lists two models.
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
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
if (args[0] === "models") {
  console.log("Fetching available models...\nfast-1\tFast One\nsmart-2\tSmart Two");
  process.exit(0);
}
console.error("ARGS " + JSON.stringify(args));
// What agy connects to when it starts: the servers configured right now.
if (process.env.FAKE_AGY_SEEN) appendFileSync(process.env.FAKE_AGY_SEEN, JSON.stringify(readConfig().mcpServers) + "\n");
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
out({ event: "init", conversation_id: id, init: { cwd: process.cwd(), tools: [], permission_mode: "request-review" } });
createInterface({ input: process.stdin }).on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.event !== "user") return;
  const text = msg.message.content;
  step({ step_index: index++, state: "DONE", step_type: "user_input" });
  const words = text.split(" ");
  if (words[0] === "edit") {
    const file = resolve(process.cwd(), words[1]);
    // As agy does: read the file, think (a model step), then edit it, writing it right
    // after announcing the edit.
    const v = index++;
    const view = { name: "view_file", parameters: { AbsolutePath: file } };
    step({ step_index: v, state: "ACTIVE", step_type: "tool", tool_name: view.name, tool_info: view });
    step({ step_index: v, state: "DONE", step_type: "tool", tool_name: view.name, tool_info: { ...view, output: "1 lines" } });
    setTimeout(() => {
      const i = index++;
      const info = { name: "replace_file_content", parameters: { TargetFile: file } };
      step({ step_index: i, state: "ACTIVE", step_type: "tool", tool_name: info.name, tool_info: info });
      writeFileSync(file, words.slice(2).join(" ") + "\n");
      step({ step_index: i, state: "DONE", step_type: "tool", tool_name: info.name, tool_info: info });
      answer(text);
    }, 300);
    return;
  } else if (words[0] === "run") {
    const i = index++;
    const info = { name: "run_command", parameters: { CommandLine: words.slice(1).join(" ") } };
    step({ step_index: i, state: "ACTIVE", step_type: "tool", tool_name: info.name, tool_info: info });
    step({ step_index: i, state: "ERROR", step_type: "tool", tool_name: info.name, tool_info: { ...info, error: { type: "TOOL_ERROR", message: "user denied permission to run command" } } });
    // As agy does: the turn ends at once, with nothing said.
    out({ event: "result", result: { conversation_id: id, status: "SUCCESS", response: "", usage: {}, denied_actions: [{ action: "command", display_name: "RunCommand" }] } });
    return;
  } else if (words[0] === "slow") {
    const i = index++;
    let n = 0;
    setInterval(() => step({ step_index: i, state: "ACTIVE", step_type: "agent_response", text_delta: `${n++} ` }), 50);
    return;
  }
  answer(text);
});

function answer(text) {
  const i = index++;
  step({ step_index: i, state: "ACTIVE", step_type: "agent_response", text_delta: "echo: " });
  step({ step_index: i, state: "DONE", step_type: "agent_response", text_delta: text, usage: { input_tokens: 10, output_tokens: 2, total_tokens: 12 } });
  out({ event: "result", result: { conversation_id: id, status: "SUCCESS", usage: { input_tokens: 10, output_tokens: 2, total_tokens: 12 } } });
}
