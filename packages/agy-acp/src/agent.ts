// The ACP agent: each session is an agy conversation (its id is the session id), run by
// one agy process in stream-json mode. Prompts go in as user messages; agy's steps come
// back as message chunks and tool calls.

import { spawnSync } from "node:child_process";
import * as acp from "@agentclientprotocol/sdk";
import { AgyProcess, agyBinary, isAuthError, type AgyEvent, type AgyUsage, type PermissionMode, type StepUpdate } from "./agy.js";
import { agyEnv } from "./credentials.js";
import { useServers } from "./mcp.js";
import { VERSION } from "./version.js";
import { FileMemory, diffs, mcpCall, outputContent, toolKind, toolPaths, toolTitle } from "./tools.js";

const START_TIMEOUT_MS = 90_000;

// agy's print mode can't ask for permission, so the default lets it work: it edits and
// runs commands without asking. `editsOnly` isn't named `acceptEdits` on purpose: clients
// that pick an agent's `acceptEdits` mode by policy (Forge does) would switch to it.
const MODES: acp.SessionMode[] = [
  { id: "bypassPermissions", name: "Full access", description: "Edits files and runs commands without asking." },
  { id: "editsOnly", name: "Edit files", description: "Reads and edits files without asking; runs no commands (agy can't ask for permission here)." },
  { id: "plan", name: "Plan", description: "Plans without changing anything." },
];

const EFFORTS = ["low", "medium", "high", "xhigh", "max"];

/** The mode new sessions start in: `FORGE_AGY_ACP_MODE` when it names one, else full access. */
export function defaultMode(): PermissionMode {
  const wanted = process.env.FORGE_AGY_ACP_MODE;
  return MODES.some((m) => m.id === wanted) ? (wanted as PermissionMode) : "bypassPermissions";
}

interface Turn {
  resolve: (response: acp.PromptResponse) => void;
  reject: (error: Error) => void;
  usage: AgyUsage;
  /** The agent said something this turn. */
  answered: boolean;
  /** Commands agy wasn't allowed to run this turn. */
  denied: string[];
}

/**
 * What to tell the user when agy stopped because the print mode denied something (it
 * then ends the turn without a word).
 */
export function deniedMessage(mode: PermissionMode, commands: string[], actions: string[]): string {
  const what = commands.length > 0 ? `to run ${commands.map((c) => `\`${c}\``).join(", ")}` : `a permission (${actions.join(", ") || "an action"})`;
  const why = mode === "editsOnly" ? "the *Edit files* mode only lets it read and edit the project's files" : mode === "plan" ? "the *Plan* mode doesn't let it change anything" : "agy denied it";
  return `agy stopped: it needed ${what}, and ${why} (agy can't ask for permission from here). Switch the session's mode to *Full access* to let it, then ask again.`;
}

interface Session {
  id: string;
  cwd: string;
  mode: PermissionMode;
  model?: string;
  effort?: string;
  additionalDirectories: string[];
  /** The client's MCP servers for the session, put in agy's config while it starts. */
  mcpServers: acp.McpServer[];
  process?: AgyProcess;
  turn?: Turn;
  /** Files each running edit step is changing, as they were when it started. */
  before: Map<number, Promise<Map<string, string | null>>>;
  /** Events handled in order: a step's end waits for its start's snapshot. */
  queue: Promise<void>;
  files: FileMemory;
}

/** The models `agy models` lists: id and name per line. */
export function parseModels(output: string): acp.SessionConfigSelectOption[] {
  return output
    .split("\n")
    .map((line) => line.split("\t"))
    .filter((parts) => parts.length >= 2 && parts[0].trim())
    .map(([value, name]) => ({ value: value.trim(), name: name.trim() }));
}

/** The text agy gets for a prompt: text blocks, and files as fenced blocks. */
export function promptText(blocks: acp.ContentBlock[]): string {
  const parts: string[] = [];
  for (const block of blocks) {
    switch (block.type) {
      case "text":
        parts.push(block.text);
        break;
      case "resource_link":
        parts.push(`@${block.uri}`);
        break;
      case "resource": {
        const resource = block.resource as { uri: string; text?: string };
        parts.push(resource.text !== undefined ? `${resource.uri}:\n\`\`\`\n${resource.text}\n\`\`\`` : `@${resource.uri}`);
        break;
      }
    }
  }
  return parts.join("\n\n");
}

function usage(u: AgyUsage): acp.Usage {
  return {
    totalTokens: u.total_tokens ?? (u.input_tokens ?? 0) + (u.output_tokens ?? 0),
    inputTokens: u.input_tokens ?? 0,
    outputTokens: u.output_tokens ?? 0,
    thoughtTokens: u.thinking_tokens ?? null,
    cachedReadTokens: u.cache_read_tokens ?? null,
  };
}

function addUsage(total: AgyUsage, step: AgyUsage | undefined) {
  if (!step) return;
  for (const key of ["input_tokens", "output_tokens", "thinking_tokens", "cache_read_tokens", "total_tokens"] as const) {
    total[key] = (total[key] ?? 0) + (step[key] ?? 0);
  }
}

export class AgyAgent implements acp.Agent {
  private sessions = new Map<string, Session>();
  private models?: Promise<acp.SessionConfigSelectOption[]>;

  constructor(private client: acp.AgentSideConnection) {}

  async initialize(_params: acp.InitializeRequest): Promise<acp.InitializeResponse> {
    const script = process.argv[1];
    const method = (id: string, name: string, description: string, flag: string): acp.AuthMethod => ({
      type: "terminal",
      id,
      name,
      description,
      args: [flag],
      // The command line, for clients that read the older `terminal-auth` form.
      _meta: { "terminal-auth": { command: process.execPath, args: [script, flag], label: name } },
    });
    return {
      protocolVersion: acp.PROTOCOL_VERSION,
      agentCapabilities: {
        loadSession: true,
        promptCapabilities: { embeddedContext: true, image: false, audio: false },
        // agy connects to them when it starts (see `mcp.ts`).
        mcpCapabilities: { http: true, sse: false },
      },
      authMethods: [
        method("gemini-api-key", "Gemini API key", "Use a key from Google AI Studio, kept in the macOS Keychain (recommended for tools other than Antigravity).", "--api-key"),
        method("google-login", "Google account", "Sign in with your Antigravity account in agy.", "--login"),
      ],
      agentInfo: { name: "@forge-ide/agy-acp", title: "Antigravity", version: VERSION },
    };
  }

  async authenticate(_params: acp.AuthenticateRequest): Promise<void> {
    // Both methods run in the client's terminal (`--api-key`, `--login`).
  }

  private listModels(): Promise<acp.SessionConfigSelectOption[]> {
    this.models ??= new Promise((resolve) => {
      const listed = spawnSync(agyBinary(), ["models"], { encoding: "utf8", env: agyEnv(), timeout: 30_000 });
      resolve(listed.status === 0 ? parseModels(listed.stdout) : []);
    });
    return this.models;
  }

  private async configOptions(session: Session): Promise<acp.SessionConfigOption[]> {
    const models = await this.listModels();
    const options: acp.SessionConfigOption[] = [];
    if (models.length > 0) {
      options.push({
        type: "select",
        id: "model",
        name: "Model",
        category: "model",
        currentValue: session.model ?? "default",
        options: [{ value: "default", name: "Default" }, ...models],
      });
    }
    options.push({
      type: "select",
      id: "effort",
      name: "Effort",
      category: "thought_level",
      currentValue: session.effort ?? "default",
      options: [{ value: "default", name: "Default" }, ...EFFORTS.map((e) => ({ value: e, name: e[0].toUpperCase() + e.slice(1) }))],
    });
    return options;
  }

  private modes(session: Session): acp.SessionModeState {
    return { currentModeId: session.mode, availableModes: MODES };
  }

  private newState(id: string, cwd: string, additionalDirectories: string[], mcpServers: acp.McpServer[]): Session {
    return { id, cwd, mode: defaultMode(), additionalDirectories, mcpServers, before: new Map(), queue: Promise.resolve(), files: new FileMemory() };
  }

  /** Starts agy for the session (continuing its conversation once it has one). */
  private async start(session: Session, conversationId?: string): Promise<AgyProcess> {
    // agy reads its MCP servers from its global config: the session's are there while it runs.
    const { release } = await useServers(session.mcpServers);
    let agy: AgyProcess;
    try {
      agy = await this.spawn(session, conversationId);
    } catch (error) {
      await release();
      throw error;
    }
    agy.on("exit", () => void release());
    session.process = agy;
    return agy;
  }

  /** Starts agy and waits for it to say it is ready. */
  private async spawn(session: Session, conversationId?: string): Promise<AgyProcess> {
    const agy = new AgyProcess(
      { cwd: session.cwd, mode: session.mode, model: session.model, effort: session.effort, conversationId, additionalDirectories: session.additionalDirectories },
      agyEnv(),
    );
    agy.on("event", (event: AgyEvent) => {
      session.queue = session.queue.then(() => this.handle(session, event)).catch(() => {});
    });
    agy.on("exit", (code: number | null) => this.ended(session, agy, code));
    let timer: NodeJS.Timeout | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error("agy didn't start in time")), START_TIMEOUT_MS);
    });
    try {
      await Promise.race([agy.ready, timeout]);
    } catch (error) {
      agy.kill();
      throw this.failure(String((error as Error).message ?? error));
    } finally {
      clearTimeout(timer);
    }
    return agy;
  }

  private failure(message: string): Error {
    return isAuthError(message) ? acp.RequestError.authRequired(undefined, `Authentication required: ${message}`) : new Error(message);
  }

  async newSession(params: acp.NewSessionRequest): Promise<acp.NewSessionResponse> {
    // The session takes the conversation's id once agy has one.
    const session = this.newState("", params.cwd, params.additionalDirectories ?? [], params.mcpServers ?? []);
    const agy = await this.start(session);
    session.id = await agy.ready;
    this.sessions.set(session.id, session);
    return { sessionId: session.id, modes: this.modes(session), configOptions: await this.configOptions(session) };
  }

  private ended(session: Session, agy: AgyProcess, code: number | null) {
    if (session.process === agy) session.process = undefined;
    const turn = session.turn;
    if (turn) {
      session.turn = undefined;
      turn.reject(this.failure(agy.failure(code)));
    }
  }

  async loadSession(params: acp.LoadSessionRequest): Promise<acp.LoadSessionResponse> {
    // agy picks the conversation back up when the next prompt starts it.
    const session = this.sessions.get(params.sessionId) ?? this.newState(params.sessionId, params.cwd, params.additionalDirectories ?? [], params.mcpServers ?? []);
    session.cwd = params.cwd;
    session.mcpServers = params.mcpServers ?? session.mcpServers;
    this.sessions.set(params.sessionId, session);
    return { modes: this.modes(session), configOptions: await this.configOptions(session) };
  }

  private session(id: string): Session {
    const session = this.sessions.get(id);
    if (!session) throw acp.RequestError.resourceNotFound(id);
    return session;
  }

  /** The next prompt restarts agy with the session's settings. */
  private restart(session: Session) {
    if (session.turn) return;
    session.process?.kill();
    session.process = undefined;
  }

  async setSessionMode(params: acp.SetSessionModeRequest): Promise<void> {
    const session = this.session(params.sessionId);
    if (!MODES.some((m) => m.id === params.modeId)) throw acp.RequestError.invalidParams(undefined, `unknown mode ${params.modeId}`);
    session.mode = params.modeId as PermissionMode;
    this.restart(session);
    await this.client.sessionUpdate({ sessionId: session.id, update: { sessionUpdate: "current_mode_update", currentModeId: session.mode } });
  }

  async setSessionConfigOption(params: acp.SetSessionConfigOptionRequest): Promise<acp.SetSessionConfigOptionResponse> {
    const session = this.session(params.sessionId);
    const value = String(params.value);
    const chosen = value === "default" ? undefined : value;
    if (params.configId === "model") session.model = chosen;
    else if (params.configId === "effort") session.effort = chosen;
    else throw acp.RequestError.invalidParams(undefined, `unknown option ${params.configId}`);
    this.restart(session);
    return { configOptions: await this.configOptions(session) };
  }

  async prompt(params: acp.PromptRequest): Promise<acp.PromptResponse> {
    const session = this.session(params.sessionId);
    if (session.turn) throw acp.RequestError.invalidRequest(undefined, "a prompt is already running in this session");
    let agy = session.process;
    if (!agy?.alive) agy = await this.start(session, session.id);
    const done = new Promise<acp.PromptResponse>((resolve, reject) => {
      session.turn = { resolve, reject, usage: {}, answered: false, denied: [] };
    });
    agy.send(promptText(params.prompt));
    return done;
  }

  async cancel(params: acp.CancelNotification): Promise<void> {
    const session = this.sessions.get(params.sessionId);
    if (!session?.turn) return;
    const turn = session.turn;
    session.turn = undefined;
    // agy ignores `interrupt`: stop the process; the next prompt continues the conversation.
    session.process?.kill();
    session.process = undefined;
    turn.resolve({ stopReason: "cancelled", usage: usage(turn.usage) });
  }

  private async handle(session: Session, event: AgyEvent): Promise<void> {
    if (event.event === "step_update") {
      await this.step(session, event.step_update);
      return;
    }
    if (event.event === "result") {
      const turn = session.turn;
      if (!turn) return;
      session.turn = undefined;
      const result = event.result;
      if (result.status === "ERROR") {
        turn.reject(this.failure(result.error || "agy failed"));
        return;
      }
      const denied = result.denied_actions ?? [];
      if (denied.length > 0 && !turn.answered) {
        const text = deniedMessage(session.mode, turn.denied, denied.map((d) => d.display_name ?? d.action ?? "").filter(Boolean));
        await this.client.sessionUpdate({ sessionId: session.id, update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text } } });
      }
      turn.resolve({ stopReason: "end_turn", usage: usage(result.usage ?? turn.usage) });
    }
  }

  private async step(session: Session, step: StepUpdate): Promise<void> {
    const sessionId = session.id;
    if (session.turn && step.state !== "ACTIVE") addUsage(session.turn.usage, step.usage);
    if (step.step_type === "agent_response") {
      if (step.text_delta) {
        if (session.turn && step.text_delta.trim()) session.turn.answered = true;
        await this.client.sessionUpdate({ sessionId, update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: step.text_delta } } });
      }
      return;
    }
    if (step.step_type !== "tool" || !step.tool_name) return;
    const name = step.tool_name;
    const info = step.tool_info;
    const toolCallId = `${sessionId}-${step.step_index}`;
    const mcp = mcpCall(name, info);
    if (step.state === "ACTIVE") {
      if (session.before.has(step.step_index)) return;
      session.before.set(step.step_index, session.files.before(name, info));
      await this.client.sessionUpdate({
        sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId,
          title: toolTitle(name, info, session.cwd),
          kind: toolKind(name),
          status: "in_progress",
          locations: toolPaths(info).map((path) => ({ path })),
          rawInput: mcp ? mcp.args : (info?.parameters ?? {}),
          // The MCP tool by the name other agents give it (`mcp__<server>__<tool>`),
          // which clients that offer their own MCP tools recognise.
          ...(mcp ? { name: `mcp__${mcp.server}__${mcp.tool}` } : {}),
        } as acp.SessionUpdate,
      });
      return;
    }
    if (step.state === "ERROR" && session.turn && /permission/i.test(info?.error?.message ?? "")) {
      const line = info?.parameters?.CommandLine;
      if (typeof line === "string") session.turn.denied.push(line);
    }
    const before = (await session.before.get(step.step_index)) ?? new Map();
    session.before.delete(step.step_index);
    const content = [...(await diffs(before)), ...outputContent(info)];
    await session.files.remember(name, info);
    await this.client.sessionUpdate({
      sessionId,
      update: { sessionUpdate: "tool_call_update", toolCallId, status: step.state === "DONE" ? "completed" : "failed", content, rawOutput: info?.output ?? info?.error ?? null },
    });
  }
}
