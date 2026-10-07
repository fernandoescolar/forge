// Saved connections (their passwords in the keychain), whether each is connected, and the
// explorer tree: what is expanded and the children loaded so far.
import { useSyncExternalStore } from 'react';
import { forge } from '@forge/api';
import type { ExtensionContext } from '@forge/api';
import { mongo, onSidecarExit, sql } from './client';
import type { ConnectParams, DbObject, Engine } from './client';

export type ConnectionConfig = {
  id: string;
  name: string;
  engine: Engine;
  host?: string;
  port?: number;
  user?: string;
  database?: string;
  file?: string;
  ssl?: 'disable' | 'prefer' | 'require';
  trustServerCertificate?: boolean;
  /**
   * MongoDB: connect with a connection string (`mongodb+srv://…` for Atlas) instead of host
   * and port. It may hold the password, so it is kept where passwords are (the keychain).
   */
  useUrl?: boolean;
  /** Keep the password in the keychain (else it is asked for on connect). */
  savePassword: boolean;
};

export type ConnectionState =
  | { status: 'disconnected' }
  | { status: 'connecting' }
  | { status: 'connected'; serverVersion: string; defaultDatabase: string | null }
  | { status: 'error'; message: string };

export type NodeKind = 'connection' | 'database' | 'schema' | 'group' | 'object';
export type TreeNode = {
  key: string;
  kind: NodeKind;
  label: string;
  connectionId: string;
  database: string | null;
  schema: string | null;
  group?: 'table' | 'view';
  object?: DbObject;
};

type Children = TreeNode[] | 'loading' | { error: string };

export const ENGINES: { value: Engine; label: string; port?: number }[] = [
  { value: 'postgres', label: 'PostgreSQL', port: 5432 },
  { value: 'mysql', label: 'MySQL', port: 3306 },
  { value: 'mariadb', label: 'MariaDB', port: 3306 },
  { value: 'mssql', label: 'SQL Server', port: 1433 },
  { value: 'sqlite', label: 'SQLite' },
  { value: 'mongodb', label: 'MongoDB', port: 27017 },
];

export const engineLabel = (e: Engine) => ENGINES.find((x) => x.value === e)?.label ?? e;
/** Engines whose databases hold schemas (else databases hold tables, or there is one database). */
const hasSchemas = (e: Engine) => e === 'postgres' || e === 'mssql';
const hasDatabases = (e: Engine) => e !== 'sqlite';
/** Documents in collections, not rows in tables. */
export const isMongo = (e: Engine | undefined) => e === 'mongodb';

let ctx: ExtensionContext;
let version = 0;
const listeners = new Set<() => void>();

const state = {
  connections: [] as ConnectionConfig[],
  status: new Map<string, ConnectionState>(),
  expanded: new Set<string>(),
  children: new Map<string, Children>(),
  selected: null as string | null,
  /** Passwords typed for connections that don't save theirs, for this session. */
  passwords: new Map<string, string>(),
};

function changed() {
  version++;
  listeners.forEach((l) => l());
}

export function useStore() {
  useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => version,
  );
  return state;
}

export async function init(context: ExtensionContext) {
  ctx = context;
  state.connections = (await ctx.storage.get<ConnectionConfig[]>('connections')) ?? [];
  onSidecarExit(() => {
    for (const id of state.status.keys()) state.status.set(id, { status: 'disconnected' });
    state.children.clear();
    changed();
  });
  changed();
}

const save = () => ctx.storage.set('connections', state.connections);

export const connection = (id: string) => state.connections.find((c) => c.id === id);
export const statusOf = (id: string): ConnectionState => state.status.get(id) ?? { status: 'disconnected' };

export async function saveConnection(config: ConnectionConfig, password: string | null) {
  const index = state.connections.findIndex((c) => c.id === config.id);
  if (index >= 0) state.connections[index] = config;
  else state.connections.push(config);
  state.connections.sort((a, b) => a.name.localeCompare(b.name));
  if (config.savePassword && password !== null) await ctx.secrets.set(`password:${config.id}`, password || null);
  if (!config.savePassword) await ctx.secrets.delete(`password:${config.id}`);
  if (password !== null) state.passwords.set(config.id, password);
  await save();
  // Reconnect with the new settings when it was connected.
  if (statusOf(config.id).status === 'connected') await disconnect(config.id);
  changed();
}

export async function deleteConnection(id: string) {
  await disconnect(id).catch(() => {});
  state.connections = state.connections.filter((c) => c.id !== id);
  await ctx.secrets.delete(`password:${id}`);
  state.passwords.delete(id);
  await save();
  changed();
}

export async function storedPassword(id: string): Promise<string | null> {
  return state.passwords.get(id) ?? (await ctx.secrets.get(`password:${id}`));
}

export function connectParams(config: ConnectionConfig, password: string | null): ConnectParams {
  const { engine, host, port, user, database, file, ssl, trustServerCertificate } = config;
  if (engine === 'sqlite') return { engine, file };
  if (isMongo(engine) && config.useUrl) return { engine, url: password ?? undefined };
  return { engine, host, port, user, password: password ?? undefined, database, ssl, trustServerCertificate };
}

/** Whether connecting needs a secret: a password, or MongoDB's connection string. */
const needsSecret = (c: ConnectionConfig) => c.engine !== 'sqlite' && !(isMongo(c.engine) && !c.useUrl && !c.user);

/** Shows the connection's form to type the password it doesn't save (set by the extension). */
let askPassword: (id: string) => void = () => {};
export const onNeedPassword = (f: (id: string) => void) => (askPassword = f);

/** Connects (once); asks for the password when it isn't saved. */
export async function ensureConnected(id: string): Promise<boolean> {
  const status = statusOf(id);
  if (status.status === 'connected') return true;
  const config = connection(id);
  if (!config) return false;
  const password = config.engine === 'sqlite' ? null : await storedPassword(id);
  if (needsSecret(config) && password === null && !config.savePassword) {
    askPassword(id);
    return false;
  }
  state.status.set(id, { status: 'connecting' });
  changed();
  try {
    const result = await sql.connect(id, connectParams(config, password));
    state.status.set(id, { status: 'connected', serverVersion: result.serverVersion, defaultDatabase: result.defaultDatabase });
    changed();
    return true;
  } catch (e) {
    state.status.set(id, { status: 'error', message: (e as Error).message });
    changed();
    forge.window.showMessage(`${config.name}: ${(e as Error).message}`, 'error');
    return false;
  }
}

export async function disconnect(id: string) {
  if (statusOf(id).status === 'connected') await sql.disconnect(id).catch(() => {});
  state.status.set(id, { status: 'disconnected' });
  for (const key of [...state.children.keys()]) if (key === id || key.startsWith(`${id}/`)) state.children.delete(key);
  for (const key of [...state.expanded]) if (key === id || key.startsWith(`${id}/`)) state.expanded.delete(key);
  changed();
}

// ------------------------------------------------------------------------------ the tree

export function select(key: string | null) {
  state.selected = key;
  changed();
}

export async function toggle(node: TreeNode) {
  if (state.expanded.has(node.key)) {
    state.expanded.delete(node.key);
    changed();
    return;
  }
  if (node.kind === 'connection' && !(await ensureConnected(node.connectionId))) return;
  state.expanded.add(node.key);
  changed();
  if (!Array.isArray(state.children.get(node.key))) await load(node);
}

export async function refresh(node: TreeNode) {
  for (const key of [...state.children.keys()]) if (key === node.key || key.startsWith(`${node.key}/`)) state.children.delete(key);
  if (state.expanded.has(node.key)) await load(node);
  else changed();
}

async function load(node: TreeNode) {
  state.children.set(node.key, 'loading');
  changed();
  try {
    state.children.set(node.key, await childrenOf(node));
  } catch (e) {
    state.children.set(node.key, { error: (e as Error).message });
  }
  changed();
}

function groups(parent: TreeNode, database: string | null, schema: string | null): TreeNode[] {
  return (['table', 'view'] as const).map((group) => ({
    key: `${parent.key}/${group}s`,
    kind: 'group',
    label: group === 'table' ? 'Tables' : 'Views',
    connectionId: parent.connectionId,
    database,
    schema,
    group,
  }));
}

async function childrenOf(node: TreeNode): Promise<TreeNode[]> {
  const config = connection(node.connectionId)!;
  const { connectionId } = node;
  switch (node.kind) {
    case 'connection': {
      if (!hasDatabases(config.engine)) return groups(node, null, null);
      const databases = await sql.listDatabases(connectionId);
      return databases.map((name) => ({ key: `${node.key}/db:${name}`, kind: 'database', label: name, connectionId, database: name, schema: null }));
    }
    case 'database': {
      if (isMongo(config.engine)) {
        const collections = await mongo.listCollections(connectionId, node.database!);
        return collections.map((c) => ({
          key: `${node.key}/${c.name}`,
          kind: 'object',
          label: c.name,
          connectionId,
          database: node.database,
          schema: null,
          object: { name: c.name, schema: null, kind: c.kind === 'view' ? 'view' : 'collection' },
        }));
      }
      if (!hasSchemas(config.engine)) return groups(node, node.database, null);
      const schemas = await sql.listSchemas(connectionId, node.database);
      return schemas.map((name) => ({ key: `${node.key}/schema:${name}`, kind: 'schema', label: name, connectionId, database: node.database, schema: name }));
    }
    case 'schema':
      return groups(node, node.database, node.schema);
    case 'group': {
      const objects = await objectsOf(node);
      return objects
        .filter((o) => o.kind === node.group)
        .map((o) => ({ key: `${node.key}/${o.name}`, kind: 'object', label: o.name, connectionId, database: node.database, schema: o.schema ?? node.schema, object: o }));
    }
    default:
      return [];
  }
}

/** Tables and views of a database or schema, loaded once for both of its groups. */
const objectCache = new Map<string, Promise<DbObject[]>>();
function objectsOf(node: TreeNode): Promise<DbObject[]> {
  const key = node.key.slice(0, node.key.lastIndexOf('/'));
  let objects = objectCache.get(key);
  if (!objects) {
    objects = sql.listObjects(node.connectionId, node.database, node.schema);
    objectCache.set(key, objects);
    // Kept just long enough for the other group, so Refresh loads it again.
    const forget = () => setTimeout(() => objectCache.delete(key), 2000);
    objects.then(forget, forget);
  }
  return objects;
}

export type VisibleRow = { node: TreeNode; depth: number; children: Children | undefined };

/** The rows of the tree to draw, in order. */
export function visibleRows(): VisibleRow[] {
  const rows: VisibleRow[] = [];
  const walk = (node: TreeNode, depth: number) => {
    const children = state.children.get(node.key);
    rows.push({ node, depth, children });
    if (state.expanded.has(node.key) && Array.isArray(children)) children.forEach((c) => walk(c, depth + 1));
  };
  for (const c of state.connections) {
    walk({ key: c.id, kind: 'connection', label: c.name, connectionId: c.id, database: null, schema: null }, 0);
  }
  return rows;
}

export const isExpanded = (key: string) => state.expanded.has(key);

/** Where a new query runs: the node selected in the tree, else the first connection. */
export function currentTarget(): { connectionId: string; database: string | null } | null {
  const node = visibleRows().find((r) => r.node.key === state.selected)?.node;
  if (node) return { connectionId: node.connectionId, database: node.database };
  const first = state.connections[0];
  return first ? { connectionId: first.id, database: null } : null;
}
export const newId = () => `c${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
