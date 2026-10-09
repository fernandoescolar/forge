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

test('markdown, images and charts reach the host as their own elements', async () => {
  const { ctx, commits, flushTimers } = host();
  const api = ctx.__forge.modules['@forge-ide/api'];
  const React = ctx.__forge.modules.react;
  const clicked = [];
  api.forge.panels.register({
    id: 'media',
    title: 'Media',
    render: () =>
      React.createElement(
        api.View,
        null,
        React.createElement(api.Markdown, { text: '# Hi\n\n- one' }),
        React.createElement(api.Image, { src: '/ext/logo.png', alt: 'Logo', fit: 'cover' }),
        React.createElement(api.Chart, { kind: 'line', labels: ['Mon', 'Tue'], series: [{ name: 'Builds', values: [3, null] }], onClick: (p) => clicked.push(p) }),
      ),
  });
  await new Promise((r) => setImmediate(r));
  flushTimers();
  const created = commits.flatMap((c) => c.ops).filter((o) => o.op === 'create');
  const of = (type) => created.find((o) => o.type === type);
  assert.equal(of('markdown').props.text, '# Hi\n\n- one');
  assert.deepEqual(of('image').props, { src: '/ext/logo.png', alt: 'Logo', fit: 'cover' });
  const chart = of('chart');
  assert.deepEqual(chart.props, { kind: 'line', labels: ['Mon', 'Tue'], series: [{ name: 'Builds', values: [3, null] }] });
  assert.deepEqual(chart.events, ['onClick']);
  ctx.__forge.dispatch(chart.id, 'onClick', JSON.stringify({ index: 1, label: 'Tue' }));
  assert.deepEqual(clicked, [{ index: 1, label: 'Tue' }]);
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

test('shortcuts are written the way each system writes them', () => {
  const { ctx } = host();
  const api = ctx.__forge.modules['@forge-ide/api'];
  assert.equal(api.forge.platform, 'darwin');
  assert.equal(api.shortcut('secondary-enter'), '⌘Enter');
  assert.equal(api.shortcut('secondary-shift-p'), '⌘⇧P');
  ctx.__forgeNative.platform = 'linux';
  assert.equal(api.forge.platform, 'linux');
  assert.equal(api.forge.shortcut('secondary-enter'), 'Ctrl+Enter');
  assert.equal(api.shortcut('secondary-shift-p'), 'Ctrl+Shift+P');
});
