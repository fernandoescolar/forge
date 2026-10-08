// Entry point of dist/runtime.js, evaluated once by the extension host before any extension.
// Provides the browser-ish globals React needs on QuickJS and exposes `globalThis.__forge`,
// which both the host (dispatch/resolve/timers) and extension bundles (shared modules) use.

// Must come first: React's scheduler captures setTimeout & co. when its module is evaluated.
import { clearTimersOf, fireTimer, timersFor } from './polyfills';
import * as React from 'react';
import * as JsxRuntime from 'react/jsx-runtime';
import * as Api from './index';
import { native } from './native';

const g = globalThis as any;

/**
 * The names extensions import the API by: `@forge-ide/api`, and `@forge/api`, its name
 * before it was published, which extensions built then still use.
 */
const API_MODULES = ['@forge-ide/api', '@forge/api'];

/** Every name of the API resolves to `api` for the next extension bundle that runs. */
function provideApi(api: unknown) {
  for (const name of API_MODULES) g.__forge.modules[name] = api;
}
import { dispatchEvent, unmountPanel } from './reconciler';

// ---- extension lifecycle
type Extension = { activate?: (ctx: Api.ExtensionContext) => unknown; deactivate?: () => unknown };
const contexts = new Map<string, Api.ExtensionContext>();

g.__forge = {
  modules: { react: React, 'react/jsx-runtime': JsxRuntime, ...Object.fromEntries(API_MODULES.map((name) => [name, Api])) } as Record<string, unknown>,

  /** Before an extension's bundle runs: its imports of the API get its own `forge`. */
  prepare(id: string, path: string) {
    provideApi({ ...Api, forge: Api.forgeFor(id, path) });
  },

  /** Activates the bundle just evaluated (it assigned `globalThis.__forgeExtension`). */
  activate(id: string, path: string) {
    const ext: Extension = g.__forgeExtension ?? {};
    g.__forgeExtension = undefined; // `var` globals are non-configurable, so reset rather than delete
    const ctx = Api.createContext(id, path);
    contexts.set(id, ctx);
    (g.__forgeExtensions ??= {})[id] = ext;
    Api.setActivating(path);
    try {
      const r = ext.activate?.(ctx);
      if (r && typeof (r as Promise<unknown>).catch === 'function') (r as Promise<unknown>).catch((e) => console.error(`activate ${id}:`, e));
    } catch (e) {
      console.error(`activate ${id}:`, e);
    } finally {
      Api.setActivating(null);
      provideApi(Api);
    }
  },
  /** Unloads an extension: its subscriptions, `deactivate`, then all it registered. */
  deactivate(id: string) {
    try {
      contexts.get(id)?.subscriptions.forEach((d) => d.dispose());
      g.__forgeExtensions?.[id]?.deactivate?.();
    } catch (e) {
      console.error(`deactivate ${id}:`, e);
    }
    contexts.delete(id);
    if (g.__forgeExtensions) delete g.__forgeExtensions[id];
    Api.disposeOwned(id);
    clearTimersOf(id);
    provideApi(Api);
  },
  dispatch(id: number, event: string, payloadJson: string) {
    dispatchEvent(id, event, payloadJson ? JSON.parse(payloadJson) : undefined);
  },
  resolve: Api.resolveCall,
  webviewMessage: Api.deliverWebviewMessage,
  settingChanged: Api.deliverSettingChanged,
  event: Api.deliverEvent,
  runCommand: Api.runCommand,
  runTool: Api.runTool,
  unmountPanel,
  fireTimer,
  /** The timer functions an extension's bundle runs with (see js.rs `ToJs::Load`). */
  timersFor,
};
