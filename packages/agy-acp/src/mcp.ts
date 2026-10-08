// The session's MCP servers for agy. agy reads MCP servers only from its global config
// (`~/.gemini/config/mcp_config.json`, through `agy mcp add`): it connects to them when
// it starts, and reads the config again on every turn to tell the model which servers it
// has. So a session's servers stay in the config while an agy uses them, and come out
// when the last one ends (or this adapter does). Servers the user configured under the
// same name are left alone. A note of what is added, with the adapter's pid, lets a later
// start clean up after an adapter that died; a lock keeps adapters from mixing edits.

import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import type * as acp from "@agentclientprotocol/sdk";
import { agyBinary } from "./agy.js";

const LOCK_STALE_MS = 120_000;

function configFile(): string {
  return process.env.AGY_MCP_CONFIG || join(homedir(), ".gemini", "config", "mcp_config.json");
}

function noteFile(): string {
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "forge-agy-acp", "added-mcp-servers.json");
}

/** The names of the MCP servers in agy's config. */
export function configuredServers(): Set<string> {
  try {
    const text = readFileSync(configFile(), "utf8").trim();
    return new Set(text ? Object.keys(JSON.parse(text).mcpServers ?? {}) : []);
  } catch {
    return new Set();
  }
}

/** `agy mcp add` arguments for a server, or `undefined` when agy can't take it. */
export function addArgs(server: acp.McpServer): string[] | undefined {
  if ("type" in server && (server.type === "http" || server.type === "sse")) {
    const headers = (server.headers ?? []).flatMap((h) => ["--header", `${h.name}: ${h.value}`]);
    return ["mcp", "add", ...headers, server.name, server.url];
  }
  if ("command" in server) {
    const env = (server.env ?? []).flatMap((e) => ["--env", `${e.name}=${e.value}`]);
    return ["mcp", "add", ...env, server.name, "--", server.command, ...server.args];
  }
  return undefined;
}

function agy(args: string[]): boolean {
  return spawnSync(agyBinary(), args, { encoding: "utf8", timeout: 30_000 }).status === 0;
}

/** Which adapter (pid) added each server agy has from us. */
type Note = Record<string, number>;

function readNote(): Note {
  try {
    const note = JSON.parse(readFileSync(noteFile(), "utf8"));
    return note && typeof note === "object" && !Array.isArray(note) ? note : {};
  } catch {
    return {};
  }
}

function writeNote(note: Note) {
  mkdirSync(join(noteFile(), ".."), { recursive: true });
  if (Object.keys(note).length === 0) rmSync(noteFile(), { force: true });
  else writeFileSync(noteFile(), JSON.stringify(note));
}

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

async function lock(): Promise<() => void> {
  const dir = join(tmpdir(), "forge-agy-acp-mcp.lock");
  for (;;) {
    try {
      mkdirSync(dir);
      return () => rmSync(dir, { recursive: true, force: true });
    } catch {
      // Left by a process that died while holding it.
      try {
        if (Date.now() - statSync(dir).mtimeMs > LOCK_STALE_MS) rmSync(dir, { recursive: true, force: true });
      } catch {}
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

/** Sessions of this adapter using each server it added. */
const users = new Map<string, number>();
let exitHook = false;

/** Removes what this adapter added (when it is closing). */
function removeAll() {
  const note = readNote();
  for (const name of users.keys()) {
    if (note[name] === process.pid) {
      agy(["mcp", "remove", name]);
      delete note[name];
    }
  }
  users.clear();
  writeNote(note);
}

/**
 * Puts `servers` in agy's config for a session; the result takes them out again once no
 * session of this adapter uses them. Returns the names left out (a server of the user's
 * has the name, or agy can't take it).
 */
export async function useServers(servers: acp.McpServer[]): Promise<{ release: () => Promise<void>; skipped: string[] }> {
  if (servers.length === 0) return { release: async () => {}, skipped: [] };
  if (!exitHook) {
    exitHook = true;
    process.on("exit", removeAll);
    for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) process.on(signal, () => process.exit(0));
  }
  const unlock = await lock();
  const used: string[] = [];
  const skipped: string[] = [];
  try {
    const note = readNote();
    // Left by adapters that died without taking them out.
    for (const [name, pid] of Object.entries(note)) {
      if (pid !== process.pid && !alive(pid)) {
        agy(["mcp", "remove", name]);
        delete note[name];
      }
    }
    const existing = configuredServers();
    for (const server of servers) {
      const args = addArgs(server);
      const ours = note[server.name] !== undefined;
      if (!args || (existing.has(server.name) && !ours)) {
        skipped.push(server.name);
        continue;
      }
      // Noted before adding: if this process dies now, a later start removes it.
      note[server.name] = process.pid;
      writeNote(note);
      // Added again even when there: the newest session's address is the one agy shows.
      if (agy(args)) {
        used.push(server.name);
        users.set(server.name, (users.get(server.name) ?? 0) + 1);
      } else {
        skipped.push(server.name);
      }
    }
    writeNote(note);
  } finally {
    unlock();
  }
  let released = false;
  const release = async () => {
    if (released) return;
    released = true;
    const unlock = await lock();
    try {
      const note = readNote();
      for (const name of used) {
        const left = (users.get(name) ?? 1) - 1;
        if (left > 0) {
          users.set(name, left);
          continue;
        }
        users.delete(name);
        if (note[name] === process.pid) {
          agy(["mcp", "remove", name]);
          delete note[name];
        }
      }
      writeNote(note);
    } finally {
      unlock();
    }
  };
  return { release, skipped };
}
