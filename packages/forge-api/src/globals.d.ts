// Globals provided by src/runtime.ts on top of QuickJS (there is no DOM or Node here).
declare function setTimeout(fn: (...args: any[]) => void, ms?: number, ...args: any[]): number;
declare function clearTimeout(id?: number): void;
declare function setInterval(fn: () => void, ms?: number): number;
declare function clearInterval(id?: number): void;
declare function queueMicrotask(fn: () => void): void;
declare const console: { log(...a: unknown[]): void; info(...a: unknown[]): void; debug(...a: unknown[]): void; warn(...a: unknown[]): void; error(...a: unknown[]): void };
