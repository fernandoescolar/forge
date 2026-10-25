// The adapter's PreToolUse hook for agy. agy 1.x only applies hooks from the project's
// `.agents/hooks.json` or from plugins, so the adapter installs its hook (`hook.ts`) as a
// plugin of its own while a session uses it, and uninstalls it when the last one (of any
// adapter of this install) ends. The plugin is the same for every session: what the hook
// does comes from the environment of the agy that runs it (see `hookEnv`), so sessions
// and adapters don't overwrite each other's settings, and agy runs the adapter didn't
// start get no opinion from it. Its name is per install (`forge-guard-<hash>`), so two
// installs (two Forge versions, an npx copy…) don't clobber each other's plugin either.
// A note of the adapters using each plugin lets a later start clean up after one that died.

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { agyBinary } from "./agy.js";

/** The plugin older adapters installed (one for everyone, the newest session's command). */
export const LEGACY_PLUGIN = "forge-guard";
/** How long agy waits for the hook: the user may take a while to answer. */
export const HOOK_TIMEOUT_S = 3_600;
/** A lock older than this was left by a process that died holding it. */
const LOCK_STALE_MS = 60_000;
/** How long to wait for another adapter's lock before going on without the plugin. */
const LOCK_WAIT_MS = 20_000;

function home(): string {
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "forge-agy-acp");
}

/** The client's hook command for a session, from its `_meta`. */
export function shellHookOf(meta: unknown): string | undefined {
  const command = (meta as { forge?: { shellHook?: { command?: unknown } } } | undefined)?.forge?.shellHook?.command;
  return typeof command === "string" && command.trim() ? command : undefined;
}

/** The hook script (`dist/hook.js`, next to this file). */
export function hookScript(): string {
  return join(dirname(fileURLToPath(import.meta.url)), "hook.js");
}

/** This install's plugin: one per node and script, so installs don't share it. */
export function pluginName(node = process.execPath, script = hookScript()): string {
  return `${LEGACY_PLUGIN}-${createHash("sha256").update(`${node}\n${script}`).digest("hex").slice(0, 10)}`;
}

/** The command agy runs for the plugin. */
export function hookCommand(name = pluginName(), node = process.execPath, script = hookScript()): string {
  return `"${node}" "${script}" ${name}`;
}

/** agy's hooks running `command` before each tool. */
export function hooksJson(name: string, command: string) {
  return { [name]: { PreToolUse: [{ matcher: "*", hooks: [{ type: "command", command, timeout: HOOK_TIMEOUT_S }] }] } };
}

/** What an agy process needs in its environment for the hook to act for it. */
export interface HookSettings {
  /** The adapter's permission server (see `permissions.ts`), when it asks the client. */
  socket?: string;
  /** Which session's agy this is, for the permission server. */
  token?: string;
  /** The client's command to run before shell commands. */
  shellHook?: string;
}

export function hookEnv(name: string, settings: HookSettings): Record<string, string> {
  const env: Record<string, string> = { FORGE_AGY_ACP_HOOK: name };
  if (settings.socket) env.FORGE_AGY_ACP_SOCKET = settings.socket;
  if (settings.token) env.FORGE_AGY_ACP_TOKEN = settings.token;
  if (settings.shellHook) env.FORGE_AGY_ACP_SHELL_HOOK = settings.shellHook;
  return env;
}

function agySync(args: string[]): boolean {
  return spawnSync(agyBinary(), args, { encoding: "utf8", timeout: 30_000 }).status === 0;
}

function agy(args: string[]): Promise<boolean> {
  return new Promise((resolve) => {
    const child = spawn(agyBinary(), args, { stdio: "ignore" });
    const timer = setTimeout(() => child.kill(), 30_000);
    child.on("error", () => resolve(false));
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve(code === 0);
    });
  });
}

/** The adapters (pids) using each plugin. */
type Note = Record<string, number[]>;

const noteFile = () => join(home(), "hook-users.json");

function readJson(path: string): unknown {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return undefined;
  }
}

function readNote(): Note {
  const note = readJson(noteFile());
  if (!note || typeof note !== "object" || Array.isArray(note)) return {};
  const clean: Note = {};
  for (const [name, pids] of Object.entries(note)) {
    if (Array.isArray(pids)) clean[name] = pids.filter((p) => Number.isInteger(p));
  }
  return clean;
}

/** Written whole and renamed into place: a reader never sees half of it. */
function writeAtomic(path: string, text: string) {
  mkdirSync(dirname(path), { recursive: true });
  const temp = `${path}.${process.pid}.tmp`;
  writeFileSync(temp, text);
  renameSync(temp, path);
}

function writeNote(note: Note) {
  for (const [name, pids] of Object.entries(note)) if (pids.length === 0) delete note[name];
  if (Object.keys(note).length === 0) rmSync(noteFile(), { force: true });
  else writeAtomic(noteFile(), JSON.stringify(note));
}

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

const lockDir = () => join(home(), "hooks.lock");

/** Takes the lock if it is free (or its owner is gone); the result releases it. */
function tryLock(): (() => void) | undefined {
  const dir = lockDir();
  try {
    mkdirSync(dirname(dir), { recursive: true });
    mkdirSync(dir);
  } catch {
    const owner = Number(readJson(join(dir, "owner")));
    let stale = false;
    try {
      stale = (Number.isInteger(owner) && owner > 0 && !alive(owner)) || Date.now() - statSync(dir).mtimeMs > LOCK_STALE_MS;
    } catch {}
    if (stale) rmSync(dir, { recursive: true, force: true });
    return undefined;
  }
  writeFileSync(join(dir, "owner"), String(process.pid));
  return () => {
    if (Number(readJson(join(dir, "owner"))) === process.pid) rmSync(dir, { recursive: true, force: true });
  };
}

/** The lock between adapters editing agy's plugins, waited for with a growing pause. */
async function lock(): Promise<() => void> {
  const until = Date.now() + LOCK_WAIT_MS;
  for (let pause = 20; ; pause = Math.min(pause * 2, 500)) {
    const unlock = tryLock();
    if (unlock) return unlock;
    if (Date.now() > until) throw new Error("another adapter holds the agy plugin lock");
    await new Promise((r) => setTimeout(r, pause));
  }
}

/** Sessions of this adapter running with the plugin. */
let users = 0;
let exitHook = false;

/** Drops dead adapters from the note, uninstalling plugins nobody uses any more but `keep`. */
async function sweep(note: Note, keep: string): Promise<void> {
  for (const [name, pids] of Object.entries(note)) {
    note[name] = pids.filter((p) => (p === process.pid ? users > 0 : alive(p)));
    if (note[name].length === 0 && name !== keep) await agy(["plugin", "uninstall", name]);
  }
  // The older adapters' plugin, once none of them runs.
  const legacy = join(home(), "guard-users.json");
  const old = readJson(legacy);
  if (Array.isArray(old) && !old.some((p) => Number.isInteger(p) && p !== process.pid && alive(p))) {
    await agy(["plugin", "uninstall", LEGACY_PLUGIN]);
    rmSync(legacy, { force: true });
    rmSync(join(home(), LEGACY_PLUGIN), { recursive: true, force: true });
  }
}

/** Takes this adapter off the note when it ends, uninstalling the plugin if it was the last. */
function leaveAtExit(name: string) {
  // No waiting at exit: the lock if it is free, else on without it.
  const unlock = tryLock();
  try {
    const note = readNote();
    const pids = (note[name] ?? []).filter((p) => p !== process.pid && alive(p));
    note[name] = pids;
    writeNote(note);
    if (pids.length === 0) agySync(["plugin", "uninstall", name]);
  } finally {
    unlock?.();
  }
}

/**
 * Installs this install's plugin for a session's agy; `release` takes it out again once
 * no session uses it. `installed` is false when agy couldn't take it (then nothing asks
 * for permission, and agy's print mode denies what needs one).
 */
export async function useHook(): Promise<{ name: string; installed: boolean; release: () => Promise<void> }> {
  const name = pluginName();
  if (!exitHook) {
    exitHook = true;
    process.on("exit", () => {
      if (users > 0) leaveAtExit(name);
    });
    // Signals end the process without "exit" unless they are turned into it.
    for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
      if (process.listenerCount(signal) === 0) process.on(signal, () => process.exit(0));
    }
  }
  let unlock: () => void;
  try {
    unlock = await lock();
  } catch {
    return { name, installed: false, release: async () => {} };
  }
  try {
    const note = readNote();
    await sweep(note, name);
    // Noted before installing: if this process dies now, a later start uninstalls it.
    note[name] = [...(note[name] ?? []).filter((p) => p !== process.pid), process.pid];
    writeNote(note);
    const dir = join(home(), "plugins", name);
    mkdirSync(dir, { recursive: true });
    writeAtomic(join(dir, "plugin.json"), JSON.stringify({ name, version: "1.0.0", description: "Forge's agy adapter: asks for permission, and guards shell commands" }));
    writeAtomic(join(dir, "hooks.json"), JSON.stringify(hooksJson(name, hookCommand(name)), null, 2));
    // Installed again each time: a copy someone uninstalled comes back.
    if (!(await agy(["plugin", "install", dir]))) {
      if (users === 0) {
        note[name] = note[name].filter((p) => p !== process.pid);
        writeNote(note);
      }
      return { name, installed: false, release: async () => {} };
    }
    users += 1;
  } finally {
    unlock();
  }
  let released = false;
  const release = async () => {
    if (released) return;
    released = true;
    users -= 1;
    if (users > 0) return;
    let unlock: () => void;
    try {
      unlock = await lock();
    } catch {
      return;
    }
    try {
      // Another session may have started meanwhile.
      if (users > 0) return;
      const note = readNote();
      note[name] = (note[name] ?? []).filter((p) => p !== process.pid && alive(p));
      writeNote(note);
      if (note[name] === undefined || note[name].length === 0) await agy(["plugin", "uninstall", name]);
    } finally {
      unlock();
    }
  };
  return { name, installed: true, release };
}
