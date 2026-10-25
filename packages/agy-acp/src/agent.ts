// The ACP agent: each session is an agy conversation (its id is the session id), run by
// one agy process in stream-json mode. Prompts go in as user messages; agy's steps come
// back as message chunks and tool calls; agy's tools ask the client for permission from
// the adapter's hook (see `permissions.ts`).

import { execFile } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as acp from "@agentclientprotocol/sdk";
import { AgyProcess, agyBinary, isAuthError, type AgyEvent, type AgyUsage, type PermissionMode, type StepUpdate, type ToolInfo } from "./agy.js";
import { agyEnv } from "./credentials.js";
import { hookEnv, shellHookOf, useHook } from "./hooks.js";
import { useServers } from "./mcp.js";
import { PermissionServer, alwaysKey, asks, commandOf, proposedDiff, type HookAnswer, type HookInput } from "./permissions.js";
import { VERSION } from "./version.js";
import { FileMemory, diffs, mcpCall, outputContent, toolKind, toolPaths, toolTitle } from "./tools.js";

const START_TIMEOUT_MS = 90_000;

const MODES: acp.SessionMode[] = [
  { id: "default", name: "Ask", description: "Asks before editing files or running commands." },
  { id: "acceptEdits", name: "Edit files", description: "Edits the project's files without asking; asks before running commands." },
  { id: "bypassPermissions", name: "Full access", description: "Edits files and runs commands without asking." },
  { id: "plan", name: "Plan", description: "Plans without changing anything." },
];

/** Modes by their older names. */
const ALIASES: Record<string, PermissionMode> = { editsOnly: "acceptEdits" };

function modeOf(id: string | undefined): PermissionMode | undefined {
  if (id && ALIASES[id]) return ALIASES[id];
  return MODES.some((m) => m.id === id) ? (id as PermissionMode) : undefined;
}

/** Modes in which the hook asks the client. */
const asking = (mode: PermissionMode) => mode === "default" || mode === "acceptEdits";

const EFFORTS = ["low", "medium", "high", "xhigh", "max"];

/** The mode new sessions start in: `FORGE_AGY_ACP_MODE` when it names one, else asking. */
export function defaultMode(): PermissionMode {
  return modeOf(process.env.FORGE_AGY_ACP_MODE) ?? "default";
}

/** Where prompts' images are saved for agy to look at (`view_file`). */
function attachments(): string {
  return join(tmpdir(), `forge-agy-acp-${process.pid}`);
}

const EXTENSIONS: Record<string, string> = { "image/png": "png", "image/jpeg": "jpg", "image/gif": "gif", "image/webp": "webp", "image/bmp": "bmp", "image/svg+xml": "svg" };

let saved = 0;

/** Saves the prompt's images as files agy can read; their paths, in order. */
export function saveImages(blocks: acp.ContentBlock[], dir = attachments()): string[] {
  const paths: string[] = [];
  for (const block of blocks) {
    if (block.type !== "image") continue;
    if (saved === 0) process.on("exit", () => rmSync(dir, { recursive: true, force: true }));
    mkdirSync(dir, { recursive: true });
    const path = join(dir, `image-${++saved}.${EXTENSIONS[block.mimeType] ?? "png"}`);
    writeFileSync(path, Buffer.from(block.data, "base64"));
    paths.push(path);
  }
  return paths;
}

/** The command that runs this adapter, for messages that tell the user what to type. */
const SELF = "npx -y @forge-ide/agy-acp";

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
  const why =
    mode === "plan"
      ? "the *Plan* mode doesn't let it change anything"
      : asking(mode)
        ? "agy can't ask for permission from here (the adapter couldn't install its permission hook as an agy plugin)"
        : "agy denied it";
  return `agy stopped: it needed ${what}, and ${why}. Switch the session's mode to *Full access* to let it, then ask again.`;
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
  /** The client's command to run before agy's shell commands (see `hooks.ts`). */
  shellHook?: string;
  /** Names the session to the permission server (agy has no id for it yet at first). */
  token: string;
  /** The running agy asks the client from the hook (else its print mode denies). */
  hooked: boolean;
  /** What the user allowed for the rest of the session (see `alwaysKey`). */
  always: Set<string>;
  process?: AgyProcess;
  /** A cancelled agy, until it has ended. */
  stopping?: Promise<void>;
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

/** The text agy gets for a prompt: text blocks, files as fenced blocks, and images as the
 *  files they were saved to (`images`, see `saveImages`), which agy views. */
export function promptText(blocks: acp.ContentBlock[], images: string[] = []): string {
  const parts: string[] = [];
  let image = 0;
  for (const block of blocks) {
    switch (block.type) {
      case "image": {
        const path = images[image++];
        if (path) parts.push(`[Attached image: ${path} (look at it with view_file)]`);
        break;
      }
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

/** `agy models`, without blocking: whether it worked, and what it printed. */
function agyModels(): Promise<{ ok: boolean; output: string }> {
  return new Promise((resolve) => {
    execFile(agyBinary(), ["models"], { encoding: "utf8", env: agyEnv(), timeout: 15_000 }, (error, stdout, stderr) => {
      resolve({ ok: !error, output: error ? `${stderr}\n${stdout}`.trim() || error.message : stdout });
    });
  });
}

function addUsage(total: AgyUsage, step: AgyUsage | undefined) {
  if (!step) return;
  for (const key of ["input_tokens", "output_tokens", "thinking_tokens", "cache_read_tokens", "total_tokens"] as const) {
    total[key] = (total[key] ?? 0) + (step[key] ?? 0);
  }
}

export class AgyAgent implements acp.Agent {
  private sessions = new Map<string, Session>();
  /** Sessions by their token, for the permission server. */
  private byToken = new Map<string, Session>();
  private models?: Promise<acp.SessionConfigSelectOption[]>;
  private permissions = new PermissionServer((token, input) => this.permit(token, input));

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
        // Images are saved as files agy looks at (see `saveImages`).
        promptCapabilities: { embeddedContext: true, image: true, audio: false },
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

  /**
   * Both methods sign in in a terminal (`--api-key`, `--login`), which clients run
   * themselves; this checks that it's done, and says what to run when it isn't.
   */
  async authenticate(params: acp.AuthenticateRequest): Promise<void> {
    if (params.methodId === "gemini-api-key") {
      const env = agyEnv();
      if (env.GEMINI_API_KEY || env.GOOGLE_API_KEY) return;
      throw acp.RequestError.authRequired(undefined, `No Gemini API key yet: run \`${SELF} --api-key\` in a terminal to add one, then try again.`);
    }
    if (params.methodId === "google-login") {
      const listed = await agyModels();
      if (listed.ok) return;
      const why = isAuthError(listed.output) ? "agy isn't signed in" : `agy failed: ${listed.output.trim().split("\n").pop() || "no answer"}`;
      throw acp.RequestError.authRequired(undefined, `${why}. Run \`${SELF} --login\` (or \`agy\`) in a terminal to sign in with your Google account, then try again.`);
    }
    throw acp.RequestError.invalidParams(undefined, `unknown sign-in method ${params.methodId}`);
  }

  /** agy's models; a failed listing isn't kept, so the next session asks again. */
  private listModels(): Promise<acp.SessionConfigSelectOption[]> {
    this.models ??= agyModels().then(({ ok, output }) => {
      const models = ok ? parseModels(output) : [];
      if (models.length === 0) this.models = undefined;
      return models;
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
    const token = randomBytes(12).toString("hex");
    const session: Session = {
      id,
      cwd,
      mode: defaultMode(),
      additionalDirectories,
      mcpServers,
      token,
      hooked: false,
      always: new Set(),
      before: new Map(),
      queue: Promise.resolve(),
      files: new FileMemory(),
    };
    this.byToken.set(token, session);
    return session;
  }

  /** Starts agy for the session (continuing its conversation once it has one). */
  private async start(session: Session, conversationId?: string): Promise<AgyProcess> {
    // agy reads its MCP servers and hooks from its global config: the session's are there while it runs.
    const { release: releaseServers } = await useServers(session.mcpServers);
    const ask = asking(session.mode);
    const hook = ask || session.shellHook ? await useHook() : undefined;
    const release = async () => {
      await releaseServers();
      await hook?.release();
    };
    const env: NodeJS.ProcessEnv = agyEnv();
    session.hooked = false;
    if (hook?.installed) {
      let socket: string | undefined;
      try {
        socket = ask ? await this.permissions.address() : undefined;
      } catch (error) {
        console.error(`agy-acp: can't ask for permissions: ${(error as Error).message}`);
      }
      session.hooked = socket !== undefined;
      Object.assign(env, hookEnv(hook.name, { socket, token: session.token, shellHook: session.shellHook }));
    }
    let agy: AgyProcess;
    try {
      agy = await this.spawn(session, env, conversationId);
    } catch (error) {
      await release();
      throw error;
    }
    agy.on("exit", () => void release());
    session.process = agy;
    return agy;
  }

  /** Starts agy and waits for it to say it is ready. */
  private async spawn(session: Session, env: NodeJS.ProcessEnv, conversationId?: string): Promise<AgyProcess> {
    const dirs = [...session.additionalDirectories, attachments()];
    mkdirSync(attachments(), { recursive: true });
    const agy = new AgyProcess(
      { cwd: session.cwd, mode: session.mode, hooked: session.hooked, model: session.model, effort: session.effort, conversationId, additionalDirectories: dirs },
      env,
    );
    agy.on("event", (event: AgyEvent) => {
      if (agy.stopped) return;
      session.queue = session.queue
        .then(() => this.handle(session, event))
        .catch((error) => console.error(`agy-acp: couldn't pass on agy's ${event.event} event: ${(error as Error)?.stack ?? error}`));
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
    session.shellHook = shellHookOf(params._meta);
    const agy = await this.start(session);
    session.id = await agy.ready;
    this.sessions.set(session.id, session);
    return { sessionId: session.id, modes: this.modes(session), configOptions: await this.configOptions(session) };
  }

  private ended(session: Session, agy: AgyProcess, code: number | null) {
    // One the adapter stopped (cancelled, restarted) has no turn of its own.
    if (session.process !== agy) return;
    session.process = undefined;
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
    session.shellHook = shellHookOf(params._meta) ?? session.shellHook;
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
    const mode = modeOf(params.modeId);
    if (!mode) throw acp.RequestError.invalidParams(undefined, `unknown mode ${params.modeId}`);
    const before = session.mode;
    session.mode = mode;
    // Between the asking modes the hook reads the mode as it goes: agy runs on.
    if (!(session.hooked && asking(before) && asking(mode))) this.restart(session);
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
    // A cancelled agy saves the conversation as it ends: the next one continues it.
    if (session.stopping) await session.stopping;
    let agy = session.process;
    if (!agy?.alive) agy = await this.start(session, session.id);
    const done = new Promise<acp.PromptResponse>((resolve, reject) => {
      session.turn = { resolve, reject, usage: {}, answered: false, denied: [] };
    });
    agy.send(promptText(params.prompt, saveImages(params.prompt)));
    return done;
  }

  async cancel(params: acp.CancelNotification): Promise<void> {
    const session = this.sessions.get(params.sessionId);
    if (!session?.turn) return;
    const turn = session.turn;
    session.turn = undefined;
    // agy ignores `interrupt`, but ends the turn and itself on SIGINT (killed if it doesn't);
    // the next prompt continues the conversation.
    const agy = session.process;
    session.process = undefined;
    if (agy) {
      const stopping = agy.interrupt();
      session.stopping = stopping;
      void stopping.then(() => {
        if (session.stopping === stopping) session.stopping = undefined;
      });
    }
    turn.resolve({ stopReason: "cancelled", usage: usage(turn.usage) });
  }

  /** Answers agy's hook before a tool runs: asks the client when the session's mode says so. */
  private async permit(token: string, input: HookInput): Promise<HookAnswer> {
    const session = this.byToken.get(token);
    if (!session) return { decision: "deny", reason: "This agy isn't one of the adapter's sessions." };
    const name = input.toolCall?.name ?? "";
    const args = input.toolCall?.args ?? {};
    if (!asks(session.mode, name, args, [session.cwd, ...session.additionalDirectories])) return {};
    const key = alwaysKey(name, args);
    if (session.always.has(key)) return { decision: "allow" };
    const turn = session.turn;
    if (!turn) return { decision: "deny", reason: "The turn was cancelled." };
    // The tool call is shown before it is asked about.
    await session.queue;
    const info: ToolInfo = { name, parameters: args };
    const mcp = mcpCall(name, info);
    const line = commandOf(args);
    const rawInput = mcp ? mcp.args : line !== undefined ? { ...args, command: line, cwd: args.Cwd ?? session.cwd } : args;
    const title = toolTitle(name, info, session.cwd);
    const response = await this.client.requestPermission({
      sessionId: session.id,
      toolCall: {
        toolCallId: `${session.id}-${input.stepIdx ?? name}`,
        title,
        kind: toolKind(name),
        locations: toolPaths(info).map((path) => ({ path })),
        rawInput,
        content: await proposedDiff(name, args),
        ...(mcp ? { name: `mcp__${mcp.server}__${mcp.tool}` } : {}),
      } as acp.ToolCallUpdate,
      options: [
        { optionId: "allow", name: "Allow", kind: "allow_once" },
        { optionId: "always", name: line !== undefined ? `Always allow \`${line}\`` : "Always allow in this session", kind: "allow_always" },
        { optionId: "reject", name: "Reject", kind: "reject_once" },
      ],
    });
    if (session.turn !== turn || response.outcome.outcome === "cancelled") return { decision: "deny", reason: "The user cancelled the turn." };
    if (response.outcome.optionId === "always") session.always.add(key);
    if (response.outcome.optionId === "allow" || response.outcome.optionId === "always") return { decision: "allow" };
    return { decision: "deny", reason: "The user rejected this tool call." };
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
      try {
        const denied = result.denied_actions ?? [];
        if (denied.length > 0 && !turn.answered) {
          const text = deniedMessage(session.mode, turn.denied, denied.map((d) => d.display_name ?? d.action ?? "").filter(Boolean));
          await this.client.sessionUpdate({ sessionId: session.id, update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text } } });
        }
      } finally {
        turn.resolve({ stopReason: "end_turn", usage: usage(result.usage ?? turn.usage) });
      }
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
