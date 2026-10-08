// agy's tools as ACP tool calls: their kind, a title, the files they touch, and for
// the tools that edit files, a diff of the whole file (read before and after the step:
// agy reports only which file it edits).

import { realpathSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { isAbsolute, relative } from "node:path";
import type * as acp from "@agentclientprotocol/sdk";
import type { ToolInfo } from "./agy.js";

const KINDS: Record<string, acp.ToolKind> = {
  view_file: "read",
  read_resource: "read",
  list_dir: "search",
  find_by_name: "search",
  grep_search: "search",
  write_to_file: "edit",
  replace_file_content: "edit",
  multi_replace_file_content: "edit",
  sed_file: "edit",
  notebook_edit: "edit",
  run_command: "execute",
  send_command_input: "execute",
  command_status: "execute",
  read_url_content: "fetch",
  search_web: "fetch",
  open_browser_url: "fetch",
  read_browser_page: "fetch",
};

export function toolKind(name: string): acp.ToolKind {
  return KINDS[name] ?? "other";
}

/** The absolute file paths a tool's parameters name (`TargetFile`, `AbsolutePath`…). */
export function toolPaths(info: ToolInfo | undefined): string[] {
  const paths: string[] = [];
  for (const [key, value] of Object.entries(info?.parameters ?? {})) {
    if (typeof value === "string" && /(file|path)$/i.test(key) && isAbsolute(value)) paths.push(value);
  }
  return paths;
}

function command(info: ToolInfo | undefined): string | undefined {
  const value = info?.parameters?.CommandLine ?? info?.parameters?.Command;
  return typeof value === "string" ? value : undefined;
}

function realPath(path: string): string {
  try {
    return realpathSync(path);
  } catch {
    return path;
  }
}

/** A call to an MCP server's tool (agy's `call_mcp_tool`): the server, the tool and its arguments. */
export function mcpCall(name: string, info: ToolInfo | undefined): { server: string; tool: string; args: Record<string, unknown> } | undefined {
  if (name !== "call_mcp_tool") return undefined;
  const params = info?.parameters ?? {};
  const server = params.ServerName;
  const tool = params.ToolName;
  if (typeof server !== "string" || typeof tool !== "string") return undefined;
  const args = params.Arguments;
  return { server, tool, args: args && typeof args === "object" ? (args as Record<string, unknown>) : {} };
}

/** agy reads an MCP tool's schema from `…/antigravity-cli/mcp/<server>/<tool>.json` before calling it. */
function schemaLookup(path: string): string | undefined {
  const found = /antigravity-cli\/mcp\/([^/]+)\/([^/]+)\.json$/.exec(path);
  return found ? `Look up the ${found[2]} tool (${found[1]})` : undefined;
}

/** What the tool call says in a client's list. */
export function toolTitle(name: string, info: ToolInfo | undefined, cwd: string): string {
  // agy reports real paths (/private/tmp…); the project may be opened through a link (/tmp…).
  const roots = [cwd, realPath(cwd)];
  const shown = (p: string) => {
    for (const root of roots) {
      const rel = relative(root, p);
      if (rel && !rel.startsWith("..") && !isAbsolute(rel)) return rel;
    }
    return p;
  };
  const path = toolPaths(info)[0];
  const params = info?.parameters ?? {};
  const mcp = mcpCall(name, info);
  if (mcp) return `${mcp.tool} (${mcp.server})`;
  switch (toolKind(name)) {
    case "read":
      return path ? (schemaLookup(path) ?? `Read ${shown(path)}`) : "Read";
    case "edit":
      return path ? `Edit ${shown(path)}` : "Edit";
    case "execute": {
      const line = command(info);
      return line ? `Run \`${line}\`` : "Run a command";
    }
    case "search": {
      const query = params.Query ?? params.Pattern ?? params.SearchPattern;
      const where = path ? ` in ${shown(path)}` : params.SearchPath ? ` in ${shown(String(params.SearchPath))}` : "";
      return typeof query === "string" ? `Search \`${query}\`${where}` : `List ${path ? shown(path) : "files"}`;
    }
    case "fetch": {
      const url = params.Url ?? params.URL ?? params.Query;
      return typeof url === "string" ? `Fetch ${url}` : "Fetch";
    }
    default:
      return name.replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
  }
}

/** Reads `path`, `null` when it doesn't exist. */
export async function readText(path: string): Promise<string | null> {
  try {
    return await readFile(path, "utf8");
  } catch {
    return null;
  }
}

/**
 * What the session last saw in each file: when agy read it, or after it edited it. agy
 * writes a file right after it announces the edit, so reading it then can be too late;
 * agents read a file before they edit it, which gives the content to diff against.
 */
export class FileMemory {
  private seen = new Map<string, string | null>();

  /** After a tool: the files it read or changed, as they are now. */
  async remember(name: string, info: ToolInfo | undefined): Promise<void> {
    const kind = toolKind(name);
    if (kind !== "read" && kind !== "edit") return;
    for (const path of toolPaths(info)) this.seen.set(path, await readText(path));
  }

  /** Before an edit: the files it will change, as last seen (else as they are now). */
  async before(name: string, info: ToolInfo | undefined): Promise<Map<string, string | null>> {
    const before = new Map<string, string | null>();
    if (toolKind(name) !== "edit") return before;
    for (const path of toolPaths(info)) before.set(path, this.seen.has(path) ? (this.seen.get(path) ?? null) : await readText(path));
    return before;
  }
}

/** Whole-file diffs of what changed since `before`. */
export async function diffs(before: Map<string, string | null>): Promise<acp.ToolCallContent[]> {
  const content: acp.ToolCallContent[] = [];
  for (const [path, oldText] of before) {
    const newText = await readText(path);
    if (newText === null || newText === oldText) continue;
    content.push({ type: "diff", path, oldText, newText });
  }
  return content;
}

/** A finished tool's output (or error) as content. */
export function outputContent(info: ToolInfo | undefined): acp.ToolCallContent[] {
  const text = info?.error?.message ?? info?.output;
  if (!text || !text.trim()) return [];
  return [{ type: "content", content: { type: "text", text } }];
}
