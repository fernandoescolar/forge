// The agy CLI in its stream-json print mode: one process per conversation, one user
// message per line in, NDJSON events out. agy 1.3 events:
//   {"event":"init","conversation_id":…,"init":{"cwd":…,"tools":[…],"permission_mode":…}}
//   {"event":"step_update","step_update":{"step_index":…,"state":"ACTIVE|DONE|ERROR","step_type":"user_input|agent_response|tool",
//     "text_delta"?:…, "tool_name"?:…, "tool_info"?:{"name","parameters","output"?,"error"?}, "usage"?:…}}
//   {"event":"result","result":{"status":"SUCCESS|ERROR","error"?:…,"usage":…}}
// The print mode can't ask for permissions (it denies what needs one) and ignores
// `interrupt`, so permissions are a mode chosen up front and cancelling restarts the
// process on the same conversation.

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import { EventEmitter } from "node:events";
import { appendFileSync } from "node:fs";

export interface AgyUsage {
  input_tokens?: number;
  output_tokens?: number;
  thinking_tokens?: number;
  cache_read_tokens?: number;
  total_tokens?: number;
}

export interface ToolInfo {
  name: string;
  parameters?: Record<string, unknown>;
  output?: string;
  error?: { type?: string; message?: string };
}

export interface StepUpdate {
  step_index: number;
  state: "ACTIVE" | "DONE" | "ERROR" | string;
  step_type: "user_input" | "agent_response" | "tool" | string;
  text_delta?: string;
  tool_name?: string;
  tool_info?: ToolInfo;
  usage?: AgyUsage;
}

export interface TurnResult {
  status: "SUCCESS" | "ERROR" | string;
  error?: string;
  response?: string;
  usage?: AgyUsage;
  /** What the print mode denied for lack of a permission it can't ask for. */
  denied_actions?: { action?: string; display_name?: string }[];
}

export type AgyEvent =
  | { event: "init"; conversation_id: string; init: { cwd?: string; permission_mode?: string } }
  | { event: "step_update"; step_update: StepUpdate }
  | { event: "result"; result: TurnResult };

/** How permissions work in a session: agy's print mode can't ask for them. */
export type PermissionMode = "bypassPermissions" | "editsOnly" | "plan";

export interface SessionOptions {
  cwd: string;
  mode: PermissionMode;
  model?: string;
  effort?: string;
  /** Continue this conversation instead of starting one. */
  conversationId?: string;
  additionalDirectories?: string[];
}

/** The agy command line for a session. */
export function agyArgs(options: SessionOptions): string[] {
  const args = ["--input-format", "stream-json", "--output-format", "stream-json"];
  switch (options.mode) {
    case "editsOnly":
      args.push("--mode", "accept-edits");
      break;
    case "plan":
      args.push("--mode", "plan");
      break;
    case "bypassPermissions":
      args.push("--dangerously-skip-permissions");
      break;
  }
  if (options.model) args.push("--model", options.model);
  if (options.effort) args.push("--effort", options.effort);
  if (options.conversationId) args.push("--conversation", options.conversationId);
  for (const dir of options.additionalDirectories ?? []) args.push("--add-dir", dir);
  const extra = process.env.AGY_EXTRA_ARGS?.trim();
  if (extra) args.push(...extra.split(/\s+/));
  return args;
}

/** The agy binary: `$AGY_BIN`, else `agy` on the PATH. */
export function agyBinary(): string {
  return process.env.AGY_BIN || "agy";
}

/** A user message as agy's stream input wants it. */
export function userMessage(text: string): string {
  return JSON.stringify({ event: "user", message: { role: "user", content: text } });
}

/** Whether an agy error means "sign in first". */
export function isAuthError(message: string): boolean {
  return /authentication (required|failed)|not authenticated|log in|login required/i.test(message);
}

/**
 * One running agy process. Emits `event` (an AgyEvent), `stderr` (a line) and `exit`
 * (code). `ready` resolves with the conversation id once agy says `init`.
 */
export class AgyProcess extends EventEmitter {
  readonly child: ChildProcessWithoutNullStreams;
  readonly ready: Promise<string>;
  private stderrTail: string[] = [];
  /** Appends to `FORGE_AGY_ACP_LOG`, when set: what went to and came from agy. */
  private trace: (direction: string, line: string) => void = () => {};
  private exited = false;

  constructor(options: SessionOptions, env: NodeJS.ProcessEnv) {
    super();
    this.child = spawn(agyBinary(), agyArgs(options), { cwd: options.cwd, env, stdio: ["pipe", "pipe", "pipe"] });
    this.ready = new Promise((resolve, reject) => {
      const onEvent = (event: AgyEvent) => {
        if (event.event === "init") {
          cleanup();
          resolve(event.conversation_id);
        } else if (event.event === "result" && event.result.status === "ERROR") {
          cleanup();
          reject(new Error(event.result.error || "agy failed to start"));
        }
      };
      const onExit = (code: number | null) => {
        cleanup();
        reject(new Error(this.failure(code)));
      };
      const cleanup = () => {
        this.off("event", onEvent);
        this.off("exit", onExit);
      };
      this.on("event", onEvent);
      this.on("exit", onExit);
    });
    // Rejections nobody awaited yet would crash the process.
    this.ready.catch(() => {});
    const log = process.env.FORGE_AGY_ACP_LOG;
    const trace = (direction: string, line: string) => {
      if (log) appendFileSync(log, `${new Date().toISOString()} ${direction} ${line}\n`);
    };
    trace("start", JSON.stringify(agyArgs(options)));
    this.trace = trace;
    createInterface({ input: this.child.stdout }).on("line", (line) => {
      trace("<", line);
      if (!line.trim()) return;
      let event: AgyEvent;
      try {
        event = JSON.parse(line);
      } catch {
        return;
      }
      this.emit("event", event);
    });
    createInterface({ input: this.child.stderr }).on("line", (line) => {
      trace("!", line);
      this.stderrTail.push(line);
      if (this.stderrTail.length > 20) this.stderrTail.shift();
      this.emit("stderr", line);
    });
    this.child.on("error", (error) => {
      this.stderrTail.push(String(error));
      this.exited = true;
      this.emit("exit", null);
    });
    this.child.on("exit", (code) => {
      this.exited = true;
      this.emit("exit", code);
    });
    this.child.stdin.on("error", () => {});
  }

  get alive(): boolean {
    return !this.exited;
  }

  /** Why it ended, from what it last printed. */
  failure(code: number | null): string {
    const tail = this.stderrTail.filter((l) => l.trim()).slice(-3).join(" ");
    if (code === null && /ENOENT/.test(tail)) return `agy was not found (${agyBinary()}); install the Antigravity CLI or set AGY_BIN`;
    return tail || `agy exited with code ${code}`;
  }

  send(text: string): void {
    this.trace(">", userMessage(text));
    this.child.stdin.write(userMessage(text) + "\n");
  }

  kill(): void {
    if (this.exited) return;
    this.child.stdin.end();
    this.child.kill("SIGTERM");
  }
}
