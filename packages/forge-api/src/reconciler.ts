// A React renderer whose "DOM" is a tree of GPUI nodes owned by the Rust host.
// Every commit is flushed as one batch of ops; event handlers stay in JS and are
// invoked when the host dispatches `(nodeId, eventName, payload)`.

import Reconciler from 'react-reconciler';
import { DefaultEventPriority, DiscreteEventPriority, LegacyRoot } from 'react-reconciler/constants.js';
import type { ReactNode } from 'react';
import { native, ROOT } from './native';
import type { NodeId, Op, Props } from './native';

type Instance = { id: NodeId; type: string; props: Props; container: Container };
type TextInstance = { id: NodeId; text: string; container: Container };
type Container = { panel: string; ops: Op[] };

let nextId = 1;
const handlers = new Map<NodeId, Props>();

/** Splits props into serialisable values and the names of function-valued (event) props. */
function split(props: Props): { props: Props; events: string[] } {
  const out: Props = {};
  const events: string[] = [];
  for (const [k, v] of Object.entries(props)) {
    if (k === 'children' || k === 'key' || k === 'ref') continue;
    if (typeof v === 'function') events.push(k);
    else if (v !== undefined) out[k] = v;
  }
  return { props: out, events };
}

function textOf(children: unknown): string | null {
  if (typeof children === 'string' || typeof children === 'number') return String(children);
  if (Array.isArray(children) && children.every((c) => typeof c === 'string' || typeof c === 'number')) return children.join('');
  return null;
}

let currentPriority = DefaultEventPriority;

const renderer = Reconciler({
  supportsMutation: true,
  supportsPersistence: false,
  supportsHydration: false,
  isPrimaryRenderer: true,
  supportsMicrotasks: true,
  scheduleMicrotask: (fn: () => void) => queueMicrotask(fn),
  scheduleTimeout: (fn: (...a: unknown[]) => void, ms?: number) => setTimeout(fn, ms) as unknown as number,
  cancelTimeout: (id: number) => clearTimeout(id),
  noTimeout: -1,
  rendererPackageName: '@forge/api',
  rendererVersion: '0.1.0',

  getRootHostContext: () => ({}),
  getChildHostContext: (ctx: {}) => ctx,
  getPublicInstance: (i: Instance) => i,
  shouldSetTextContent: (_type: string, props: Props) => textOf(props.children) !== null,

  createInstance(type: string, props: Props, container: Container) {
    const id = nextId++;
    const s = split(props);
    const text = textOf(props.children);
    // Single text children are folded into the node as a `text` prop.
    if (text !== null) s.props.text = text;
    handlers.set(id, props);
    container.ops.push({ op: 'create', id, type, props: s.props, events: s.events });
    return { id, type, props, container };
  },
  createTextInstance(text: string, container: Container) {
    const id = nextId++;
    container.ops.push({ op: 'text', id, text });
    return { id, text, container };
  },
  appendInitialChild(parent: Instance, child: Instance | TextInstance) {
    parent.container.ops.push({ op: 'append', parent: parent.id, child: child.id });
  },
  finalizeInitialChildren: () => false,

  appendChild(parent: Instance, child: Instance | TextInstance) {
    parent.container.ops.push({ op: 'append', parent: parent.id, child: child.id });
  },
  appendChildToContainer(container: Container, child: Instance | TextInstance) {
    container.ops.push({ op: 'append', parent: ROOT, child: child.id });
  },
  insertBefore(parent: Instance, child: Instance | TextInstance, before: Instance | TextInstance) {
    parent.container.ops.push({ op: 'insert', parent: parent.id, child: child.id, before: before.id });
  },
  insertInContainerBefore(container: Container, child: Instance | TextInstance, before: Instance | TextInstance) {
    container.ops.push({ op: 'insert', parent: ROOT, child: child.id, before: before.id });
  },
  removeChild(parent: Instance, child: Instance | TextInstance) {
    parent.container.ops.push({ op: 'remove', parent: parent.id, child: child.id });
  },
  removeChildFromContainer(container: Container, child: Instance | TextInstance) {
    container.ops.push({ op: 'remove', parent: ROOT, child: child.id });
  },
  detachDeletedInstance(i: Instance | TextInstance) {
    handlers.delete(i.id);
  },
  clearContainer(_container: Container) {
    // Containers start empty and are only cleared on unmount; removals are explicit ops.
  },

  commitUpdate(i: Instance, _type: string, _old: Props, next: Props) {
    i.props = next;
    handlers.set(i.id, next);
    const s = split(next);
    const text = textOf(next.children);
    if (text !== null) s.props.text = text;
    i.container.ops.push({ op: 'update', id: i.id, props: s.props, events: s.events });
  },
  commitTextUpdate(t: TextInstance, _old: string, text: string) {
    t.text = text;
    t.container.ops.push({ op: 'setText', id: t.id, text });
  },
  resetTextContent() {},
  commitMount() {},

  hideInstance(i: Instance) {
    i.container.ops.push({ op: 'update', id: i.id, props: { ...split(i.props).props, hidden: true }, events: split(i.props).events });
  },
  unhideInstance(i: Instance, props: Props) {
    i.container.ops.push({ op: 'update', id: i.id, props: split(props).props, events: split(props).events });
  },
  hideTextInstance(t: TextInstance) {
    t.container.ops.push({ op: 'setText', id: t.id, text: '' });
  },
  unhideTextInstance(t: TextInstance, text: string) {
    t.container.ops.push({ op: 'setText', id: t.id, text });
  },

  prepareForCommit: () => null,
  resetAfterCommit(container: Container) {
    if (container.ops.length === 0) return;
    const ops = container.ops.splice(0);
    native().commit(container.panel, JSON.stringify(ops));
  },
  preparePortalMount() {},

  getCurrentUpdatePriority: () => currentPriority,
  setCurrentUpdatePriority: (p: number) => { currentPriority = p; },
  resolveUpdatePriority: () => currentPriority || DefaultEventPriority,
  resolveEventType: () => null,
  resolveEventTimeStamp: () => Date.now(),
  trackSchedulerEvent() {},
  shouldAttemptEagerTransition: () => false,
  getInstanceFromNode: () => null,
  beforeActiveInstanceBlur() {},
  afterActiveInstanceBlur() {},
  prepareScopeUpdate() {},
  getInstanceFromScope: () => null,
  requestPostPaintCallback() {},
  maySuspendCommit: () => false,
  maySuspendCommitOnUpdate: () => false,
  maySuspendCommitInSyncRender: () => false,
  preloadInstance: () => true,
  startSuspendingCommit() {},
  suspendInstance() {},
  waitForCommitToBeReady: () => null,
  getSuspendedCommitReason: () => null,
  NotPendingTransition: null,
  HostTransitionContext: { $$typeof: Symbol.for('react.context'), _currentValue: null, _currentValue2: null } as any,
  resetFormInstance() {},
  bindToConsole: undefined,
} as any);

const roots = new Map<string, { root: unknown; container: Container }>();

function report(kind: string) {
  return (error: unknown) => native().log('error', `[react ${kind}] ${(error as Error)?.stack ?? String(error)}`);
}

/** Renders `element` into the native panel `panel`. Re-rendering replaces the element. */
export function renderPanel(panel: string, element: ReactNode) {
  let entry = roots.get(panel);
  if (!entry) {
    const container: Container = { panel, ops: [] };
    const root = renderer.createContainer(container, LegacyRoot, null, false, null, panel, report('uncaught'), report('caught'), report('recoverable'), () => {}, null);
    entry = { root, container };
    roots.set(panel, entry);
  }
  renderer.updateContainer(element, entry.root as any, null, null);
}

export function unmountPanel(panel: string) {
  const entry = roots.get(panel);
  if (!entry) return;
  renderer.updateContainer(null, entry.root as any, null, null);
  roots.delete(panel);
}

/**
 * Invoked by the host when the user interacts with a node. Like a DOM click or keystroke,
 * it is a discrete event: the updates it makes are rendered and committed before the next
 * event runs, so a click right after typing sees what was typed.
 */
export function dispatchEvent(id: NodeId, event: string, payload: unknown) {
  const handler = handlers.get(id)?.[event];
  if (typeof handler !== 'function') return;
  const previous = currentPriority;
  currentPriority = DiscreteEventPriority;
  try {
    renderer.batchedUpdates(() => (handler as (p: unknown) => void)(payload), undefined);
  } finally {
    currentPriority = previous;
  }
  (renderer as any).flushSyncWork?.();
}
