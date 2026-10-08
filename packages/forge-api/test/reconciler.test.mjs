// Runs dist/runtime.js against a fake native bridge, the same way the Rust host does.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { execFileSync } from 'node:child_process';

function host() {
  const commits = [];
  const calls = [];
  const timers = new Map();
  const ctx = { Promise, JSON, Object, Array, Symbol, Map, Set, WeakMap, Error, Math, Date };
  ctx.globalThis = ctx;
  ctx.__forgeNative = {
    commit: (panel, ops) => commits.push({ panel, ops: JSON.parse(ops) }),
    call: (method, args, id) => calls.push({ method, args: JSON.parse(args), id }),
    setTimer: (id, ms) => timers.set(id, ms),
    clearTimer: (id) => timers.delete(id),
    log: () => {},
  };
  vm.createContext(ctx);
  vm.runInContext(readFileSync('dist/runtime.js', 'utf8'), ctx);
  const flushTimers = () => { for (const id of [...timers.keys()]) { timers.delete(id); ctx.__forge.fireTimer(id); } };
  return { ctx, commits, calls, flushTimers };
}

test('extension renders a panel, handles a click and re-renders', async () => {
  execFileSync('node', ['bin/forge-ext.mjs', 'build', 'test/fixture'], { stdio: 'ignore' });
  const { ctx, commits, calls, flushTimers } = host();
  vm.runInContext(readFileSync('test/fixture/dist/extension.js', 'utf8'), ctx);
  ctx.__forge.activate('counter', '/ext/counter');
  await new Promise((r) => setImmediate(r));
  flushTimers();

  assert.deepEqual(calls[0], { method: 'panels.register', args: { id: 'counter', title: 'Counter', icon: 'sparkle', position: 'right', layout: 'scroll', extension: null }, id: 0 });
  const ops = commits.flatMap((c) => c.ops);
  const button = ops.find((o) => o.op === 'create' && o.type === 'button');
  assert.equal(button.props.label, 'Clicked 0 times');
  assert.deepEqual(button.events, ['onClick']);
  assert.ok(ops.some((o) => o.op === 'append' && o.parent === 0), 'root child appended');

  commits.length = 0;
  ctx.__forge.dispatch(button.id, 'onClick', '');
  await new Promise((r) => setImmediate(r));
  flushTimers();
  const update = commits.flatMap((c) => c.ops).find((o) => o.op === 'update' && o.id === button.id);
  assert.equal(update.props.label, 'Clicked 1 times');
});

test('host calls resolve promises', async () => {
  const { ctx, calls } = host();
  const api = ctx.__forge.modules['@forge-ide/api'];
  const p = api.forge.workspace.readFile('a.txt');
  const call = calls.find((c) => c.method === 'workspace.readFile');
  ctx.__forge.resolve(call.id, true, JSON.stringify('hello'));
  assert.equal(await p, 'hello');
  const q = api.forge.workspace.readFile('missing');
  ctx.__forge.resolve(calls.at(-1).id, false, JSON.stringify('not found'));
  await assert.rejects(q, /not found/);
});

test('webview panels route messages both ways', () => {
  const { ctx, calls } = host();
  const api = ctx.__forge.modules['@forge-ide/api'];
  api.setActivating('/ext/demo');
  const panel = api.forge.webviews.register({ id: 'demo', title: 'Demo', html: 'index.html' });
  api.setActivating(null);
  assert.deepEqual(calls.at(-1), { method: 'webviews.register', args: { id: 'demo', title: 'Demo', icon: 'globe', html: 'index.html', root: '/ext/demo', extension: null }, id: 0 });

  const got = [];
  panel.onMessage((m) => got.push(m));
  ctx.__forge.webviewMessage('demo', JSON.stringify({ hello: 1 }));
  assert.deepEqual(got, [{ hello: 1 }]);

  panel.postMessage({ reply: true });
  assert.deepEqual(calls.at(-1), { method: 'webviews.postMessage', args: { id: 'demo', message: { reply: true } }, id: 0 });
  assert.throws(() => api.forge.webviews.register({ id: 'x', title: 'X', html: 'a.html' }), /activate/);
});

test('events reach listeners, and the host hears of each event once', async () => {
  const { ctx, calls } = host();
  const api = ctx.__forge.modules['@forge-ide/api'];
  const seen = [];
  const a = api.forge.workspace.onDidSaveFile((p) => seen.push(['a', p]));
  api.forge.workspace.onDidSaveFile((p) => seen.push(['b', p]));
  assert.equal(calls.filter((c) => c.method === 'events.listen').length, 1);
  assert.deepEqual(calls.find((c) => c.method === 'events.listen').args, { name: 'workspace.fileSaved' });
  ctx.__forge.event('workspace.fileSaved', JSON.stringify('/p/a.ts'));
  a.dispose();
  ctx.__forge.event('workspace.fileSaved', JSON.stringify('/p/b.ts'));
  assert.deepEqual(seen, [['a', '/p/a.ts'], ['b', '/p/a.ts'], ['b', '/p/b.ts']]);
});

test('activate gets storage scoped to the extension', async () => {
  const { ctx, calls } = host();
  let context;
  ctx.__forgeExtension = { activate: (c) => { context = c; } };
  ctx.__forge.activate('notes', '/ext/notes');
  context.storage.set('count', 2);
  context.workspaceStorage.get('draft');
  const storage = calls.filter((c) => c.method.startsWith('storage.')).map((c) => [c.method, c.args]);
  assert.deepEqual(storage, [
    ['storage.set', { extension: 'notes', scope: 'global', key: 'count', value: 2 }],
    ['storage.get', { extension: 'notes', scope: 'workspace', key: 'draft' }],
  ]);
});

test('extensions built with the old name get their own forge too', () => {
  const { ctx } = host();
  ctx.__forge.prepare('old', '/ext/old');
  const current = ctx.__forge.modules['@forge-ide/api'];
  assert.equal(ctx.__forge.modules['@forge/api'], current, 'both names are the API prepared for this extension');
  assert.ok(current.forge, 'with its own forge');
  ctx.__forgeExtension = {};
  ctx.__forge.activate('old', '/ext/old');
});

test('unloading an extension takes away what it registered', async () => {
  const { ctx, calls } = host();
  const shared = ctx.__forge.modules['@forge-ide/api'];
  ctx.__forge.prepare('a', '/ext/a');
  ctx.__forgeExtension = {
    activate() {
      const { forge } = ctx.__forge.modules['@forge-ide/api'];
      forge.commands.register('a.hello', 'Hello', () => {});
      forge.panels.register({ id: 'a-panel', title: 'A', render: () => null });
      forge.workspace.onDidSaveFile(() => { throw new Error('should be gone'); });
    },
  };
  ctx.__forge.activate('a', '/ext/a');
  assert.equal(ctx.__forge.modules['@forge-ide/api'], shared, 'the shared module is back after activation');
  assert.equal(ctx.__forge.modules['@forge/api'], shared, 'extensions built with the old name get the same module');
  calls.length = 0;
  ctx.__forge.deactivate('a');
  const undone = calls.map((c) => [c.method, c.args.id]);
  assert.deepEqual(undone, [['commands.unregister', 'a.hello'], ['panels.unregister', 'a-panel']]);
  ctx.__forge.event('workspace.fileSaved', JSON.stringify('/p/x'));
  assert.throws(() => ctx.__forge.runCommand('a.hello'), /unknown command/);
});

test('intervals repeat until cleared; an extension’s timers stop when it unloads', async () => {
  const { ctx, flushTimers } = host();
  const ticks = [];
  const mine = ctx.__forge.timersFor('a');
  mine.setInterval(() => ticks.push('a'), 10);
  ctx.setInterval(() => ticks.push('shared'), 10);
  flushTimers();
  flushTimers();
  assert.deepEqual(ticks, ['a', 'shared', 'a', 'shared']);
  ctx.__forge.deactivate('a');
  flushTimers();
  assert.deepEqual(ticks.slice(4), ['shared'], 'only the extension’s timers stopped');
});
