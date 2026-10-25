// Asking the client before agy's tools run. agy's print mode can't ask, but it runs the
// adapter's PreToolUse hook (`hook.ts`, installed by `hooks.ts`) before each tool and does
// what it answers: "allow" or "deny" (with a reason agy tells the model); nothing printed
// lets agy decide. The hook asks the adapter over a local socket, and the adapter asks the
// client (`session/request_permission`), so the client's permission rules decide.

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { realpathSync, rmSync } from "node:fs";
import { createConnection, createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { isAbsolute, join, relative } from "node:path";
import type * as acp from "@agentclientprotocol/sdk";
import type { PermissionMode } from "./agy.js";
import { readText, toolKind, toolPaths } from "./tools.js";

/** agy's hook input (camelCase). */
export interface HookInput {
  toolCall?: { name?: string; args?: Record<string, unknown> };
  stepIdx?: number;
  conversationId?: string;
}

/** The hook's answer: a decision, or none to let agy decide. */
export interface HookAnswer {
  decision?: "allow" | "deny";
  reason?: string;
}

/** Tools that change nothing outside agy (or only look): never asked about. */
const QUIET = new Set([
  "ask_custom_permission",
  "ask_permission",
  "ask_question",
  "command_status",
  "finish",
  "list_permissions",
  "list_plugin_accounts",
  "list_resources",
  "list_browser_pages",
  "manage_inbox",
  "manage_subagents",
  "manage_task",
  "define_subagent",
  "invoke_subagent",
  "browser_subagent",
  "search_marketplace",
  "search_web",
  "read_browser_page",
  "browser_get_dom",
  "browser_get_network_request",
  "browser_list_network_requests",
  "capture_browser_console_logs",
  "capture_browser_screenshot",
  "wait",
  "wait_5_seconds",
]);

function realPath(path: string): string {
  try {
    return realpathSync(path);
  } catch {
    return path;
  }
}

/** Whether `path` is in one of `roots` (as given, or with links resolved: agy reports real paths). */
export function inside(path: string, roots: string[]): boolean {
  return roots.flatMap((r) => [r, realPath(r)]).some((root) => {
    const rel = relative(root, path);
    return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
  });
}

/** Whether the client is asked before `name` runs, in `mode`, in a session over `roots`. */
export function asks(mode: PermissionMode, name: string, args: Record<string, unknown>, roots: string[]): boolean {
  if (mode !== "default" && mode !== "acceptEdits") return false;
  const kind = toolKind(name);
  if (kind === "read" || kind === "search" || QUIET.has(name)) return false;
  // Edits in the project go ahead; elsewhere they are asked about.
  if (mode === "acceptEdits" && kind === "edit") return !toolPaths({ name, parameters: args }).every((p) => inside(p, roots));
  return true;
}

/** The command line a tool runs, if it runs one. */
export function commandOf(args: Record<string, unknown>): string | undefined {
  const line = args.CommandLine ?? args.Command;
  return typeof line === "string" ? line : undefined;
}

/** What "always allow" covers: the same command, or the same tool. */
export function alwaysKey(name: string, args: Record<string, unknown>): string {
  const line = commandOf(args);
  return line !== undefined ? `${name}:${line}` : name;
}

function replace(text: string, target: unknown, replacement: unknown, all: unknown): string | undefined {
  if (typeof target !== "string" || typeof replacement !== "string" || !target || !text.includes(target)) return undefined;
  return all === true ? text.split(target).join(replacement) : text.replace(target, () => replacement);
}

/** The diff an edit will make, from its arguments (none when they can't be read). */
export async function proposedDiff(name: string, args: Record<string, unknown>): Promise<acp.ToolCallContent[]> {
  const path = args.TargetFile;
  if (typeof path !== "string" || !isAbsolute(path)) return [];
  const oldText = await readText(path);
  let newText: string | undefined;
  if (name === "write_to_file") {
    newText = typeof args.CodeContent === "string" ? args.CodeContent : undefined;
  } else if (name === "replace_file_content" && oldText !== null) {
    newText = replace(oldText, args.TargetContent, args.ReplacementContent, args.AllowMultiple);
  } else if (name === "multi_replace_file_content" && oldText !== null && Array.isArray(args.ReplacementChunks)) {
    newText = oldText;
    for (const chunk of args.ReplacementChunks as Record<string, unknown>[]) {
      newText = newText === undefined ? undefined : replace(newText, chunk?.TargetContent, chunk?.ReplacementContent, chunk?.AllowMultiple);
    }
  }
  return newText === undefined || newText === oldText ? [] : [{ type: "diff", path, oldText, newText }];
}

/**
 * The adapter's end of the hook's socket: one JSON line in (`{token, input}`), one out
 * (a HookAnswer). Listens from the first time it's needed.
 */
export class PermissionServer {
  private server?: Promise<string>;

  constructor(private answer: (token: string, input: HookInput) => Promise<HookAnswer>) {}

  /** Where the hook connects. */
  address(): Promise<string> {
    this.server ??= this.listen().catch((error) => {
      this.server = undefined;
      throw error;
    });
    return this.server;
  }

  private listen(): Promise<string> {
    const name = `forge-agy-acp-${process.pid}-${randomBytes(4).toString("hex")}`;
    const path = process.platform === "win32" ? `\\\\.\\pipe\\${name}` : join(tmpdir(), `${name}.sock`);
    const server: Server = createServer((socket) => {
      let buffer = "";
      socket.setEncoding("utf8");
      socket.on("error", () => {});
      socket.on("data", (data) => {
        buffer += data;
        const end = buffer.indexOf("\n");
        if (end < 0) return;
        let request: { token?: string; input?: HookInput };
        try {
          request = JSON.parse(buffer.slice(0, end));
        } catch {
          socket.end(JSON.stringify({ decision: "deny", reason: "The permission request was malformed." }) + "\n");
          return;
        }
        this.answer(String(request.token ?? ""), request.input ?? {})
          .catch((error): HookAnswer => ({ decision: "deny", reason: `Couldn't ask for permission: ${(error as Error)?.message ?? error}` }))
          .then((answer) => socket.end(JSON.stringify(answer) + "\n"));
      });
    });
    server.unref();
    return new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(path, () => {
        if (process.platform !== "win32") process.on("exit", () => rmSync(path, { force: true }));
        resolve(path);
      });
    });
  }
}

/** Asks the adapter at `address` (the hook's side). */
export function ask(address: string, token: string, input: HookInput): Promise<HookAnswer> {
  return new Promise((resolve, reject) => {
    const socket = createConnection(address);
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("connect", () => socket.write(JSON.stringify({ token, input }) + "\n"));
    socket.on("data", (data) => (buffer += data));
    socket.on("error", reject);
    socket.on("close", () => {
      try {
        resolve(JSON.parse(buffer.trim()));
      } catch {
        reject(new Error("no answer"));
      }
    });
  });
}

/** Runs the client's shell-command hook on agy's input; its output. */
function runGuard(command: string, input: string): Promise<string> {
  return new Promise((resolve) => {
    const child = spawn(command, { shell: true, stdio: ["pipe", "pipe", "ignore"] });
    let out = "";
    const timer = setTimeout(() => child.kill(), 10_000);
    child.stdout.on("data", (d) => (out += d));
    child.on("error", () => resolve(""));
    child.on("close", () => {
      clearTimeout(timer);
      resolve(out.trim());
    });
    child.stdin.on("error", () => {});
    child.stdin.end(input);
  });
}

/**
 * What the hook prints for agy's `input`, as plugin `name`, in the environment agy gave
 * it (see `hookEnv`): nothing for agy runs that aren't this plugin's adapter's; else the
 * client's shell hook's refusal, or the adapter's answer.
 */
export async function hookAnswer(input: string, name: string, env: NodeJS.ProcessEnv = process.env): Promise<string> {
  if (env.FORGE_AGY_ACP_HOOK !== name) return "";
  let call: HookInput;
  try {
    call = JSON.parse(input);
  } catch {
    return "";
  }
  const guard = env.FORGE_AGY_ACP_SHELL_HOOK;
  if (guard && call.toolCall?.name === "run_command") {
    const out = await runGuard(guard, input);
    try {
      if (JSON.parse(out).decision === "deny") return out;
    } catch {}
  }
  const address = env.FORGE_AGY_ACP_SOCKET;
  if (!address) return "";
  try {
    const answer = await ask(address, env.FORGE_AGY_ACP_TOKEN ?? "", call);
    return answer.decision ? JSON.stringify(answer) : "";
  } catch (error) {
    // Nobody to ask: nothing runs that should have been asked about.
    return JSON.stringify({ decision: "deny", reason: `Couldn't ask the user for permission (the agy adapter didn't answer: ${(error as Error).message}).` });
  }
}
