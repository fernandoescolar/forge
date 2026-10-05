// Browser-ish globals React needs, implemented on top of the native bridge.
// Imported before React by runtime.ts.
import { native } from './native';

type Timer = { fn: () => void; every?: number; owner?: string };
const timers = new Map<number, Timer>();
let nextTimer = 1;
const g = globalThis as any;

// ---- timers & microtasks
function schedule(fn: () => void, ms: unknown, repeat: boolean, owner?: string) {
  const id = nextTimer++;
  const delay = Math.max(0, Number(ms) || 0);
  timers.set(id, { fn, every: repeat ? delay : undefined, owner });
  native().setTimer(id, delay);
  return id;
}
function clear(id: number) {
  if (timers.delete(id)) native().clearTimer(id);
}
g.setTimeout = (fn: (...a: unknown[]) => void, ms = 0, ...args: unknown[]) => schedule(() => fn(...args), ms, false);
g.clearTimeout = clear;
g.setInterval = (fn: (...a: unknown[]) => void, ms = 0, ...args: unknown[]) => schedule(() => fn(...args), ms, true);
g.clearInterval = clear;

/**
 * Timer functions whose timers belong to `owner` (an extension), so `clearTimersOf` can
 * stop them all when it unloads. Each extension's bundle runs with these in scope.
 */
export function timersFor(owner: string) {
  return {
    setTimeout: (fn: (...a: unknown[]) => void, ms = 0, ...args: unknown[]) => schedule(() => fn(...args), ms, false, owner),
    clearTimeout: clear,
    setInterval: (fn: (...a: unknown[]) => void, ms = 0, ...args: unknown[]) => schedule(() => fn(...args), ms, true, owner),
    clearInterval: clear,
  };
}

/** Stops every timer `owner` started. */
export function clearTimersOf(owner: string) {
  for (const [id, timer] of [...timers]) {
    if (timer.owner === owner) clear(id);
  }
}
g.queueMicrotask ??= (fn: () => void) => { Promise.resolve().then(fn); };

// ---- console
const fmt = (args: unknown[]) => args.map((a) => (typeof a === 'string' ? a : a instanceof Error ? `${a.name}: ${a.message}${a.stack ? `\n${a.stack}` : ''}` : JSON.stringify(a))).join(' ');
g.console = {
  log: (...a: unknown[]) => native().log('info', fmt(a)),
  info: (...a: unknown[]) => native().log('info', fmt(a)),
  debug: (...a: unknown[]) => native().log('debug', fmt(a)),
  warn: (...a: unknown[]) => native().log('warn', fmt(a)),
  error: (...a: unknown[]) => native().log('error', fmt(a)),
};


/** Called by the host when timer `id` is due. */
export function fireTimer(id: number) {
  const timer = timers.get(id);
  if (!timer) return;
  // Intervals stay and come back; timeouts are done.
  if (timer.every === undefined) timers.delete(id);
  else native().setTimer(id, timer.every);
  timer.fn();
}
