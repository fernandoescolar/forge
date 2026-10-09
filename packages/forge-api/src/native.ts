// Contract between the JS runtime and the Rust extension host (crates/forge-extension-host).
// Rust installs `globalThis.__forgeNative` before evaluating runtime.js; everything that
// crosses the boundary is a JSON string so the native side stays engine-agnostic.

export type NodeId = number;
export type Props = Record<string, unknown>;

/** Mutations of a panel's UI tree, applied in order by the native renderer. */
export type Op =
  | { op: 'create'; id: NodeId; type: string; props: Props; events: string[] }
  | { op: 'text'; id: NodeId; text: string }
  | { op: 'append'; parent: NodeId; child: NodeId }
  | { op: 'insert'; parent: NodeId; child: NodeId; before: NodeId }
  | { op: 'remove'; parent: NodeId; child: NodeId }
  | { op: 'update'; id: NodeId; props: Props; events: string[] }
  | { op: 'setText'; id: NodeId; text: string };

/** Root node id of every panel container. */
export const ROOT: NodeId = 0;

export interface Native {
  /** Apply a batch of ops to panel `panel`'s tree. */
  commit(panel: string, opsJson: string): void;
  /** Fire-and-forget or request/response host call. `callId` is 0 for no reply. */
  call(method: string, argsJson: string, callId: number): void;
  setTimer(id: number, ms: number): void;
  clearTimer(id: number): void;
  log(level: string, message: string): void;
  /** `darwin`, `linux` or `win32` (missing outside Forge). */
  platform?: string;
}

declare global {
  // eslint-disable-next-line no-var
  var __forgeNative: Native;
}

export function native(): Native {
  const n = globalThis.__forgeNative;
  if (!n) throw new Error('Forge native bridge missing: this code must run inside the Forge extension host');
  return n;
}
