// Saved connections (their passwords in the keychain), whether each is connected, and the
// explorer tree: what is expanded and the children loaded so far. A Redis database's keys
// are found with SCAN a batch at a time (by a pattern) and shown as a tree split at `:`.
import { useSyncExternalStore } from 'react';
import { forge } from '@forge-ide/api';
import type { ExtensionContext } from '@forge-ide/api';
import { mongo, onSidecarExit, redis, sql } from './client';
import type { ConnectParams, DbObject, Engine, RedisKey } from './client';

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
  /** Redis: connect through Sentinel, to the master it names (the sentinels' password is in the keychain). */
  useSentinel?: boolean;
  sentinels?: string;
  masterName?: string;
  /** A password was given (Redis and MongoDB may have none): ask for it when it isn't saved. */
  hasPassword?: boolean;
  /** Keep the password in the keychain (else it is asked for on connect). */
  savePassword: boolean;
};

export type ConnectionState =
  | { status: 'disconnected' }
  | { status: 'connecting' }
  | { status: 'connected'; serverVersion: string; defaultDatabase: string | null }
  | { status: 'error'; message: string };

/** Redis adds `namespace` (keys sharing a prefix up to a `:`), `key`, and `more` (load the next batch). */
export type NodeKind = 'connection' | 'database' | 'schema' | 'group' | 'object' | 'namespace' | 'key' | 'more';
export type TreeNode = {
  key: string;
  kind: NodeKind;
  label: string;
  connectionId: string;
  database: string | null;
  schema: string | null;
  group?: 'table' | 'view';
  object?: DbObject;
  /** Shown after the label (Redis: key counts). */
  description?: string;
  /** Redis: the key a `key` node is, the prefix a `namespace` node groups (`user:1:`). */
  redisKey?: RedisKey;
  prefix?: string;
};

type Children = TreeNode[] | 'loading' | { error: string };

export const ENGINES: { value: Engine; label: string; port?: number }[] = [
  { value: 'postgres', label: 'PostgreSQL', port: 5432 },
  { value: 'mysql', label: 'MySQL', port: 3306 },
  { value: 'mariadb', label: 'MariaDB', port: 3306 },
  { value: 'mssql', label: 'SQL Server', port: 1433 },
  { value: 'sqlite', label: 'SQLite' },
  { value: 'mongodb', label: 'MongoDB', port: 27017 },
  { value: 'redis', label: 'Redis', port: 6379 },
];

export const engineLabel = (e: Engine) => ENGINES.find((x) => x.value === e)?.label ?? e;
/** Engines whose databases hold schemas (else databases hold tables, or there is one database). */
const hasSchemas = (e: Engine) => e === 'postgres' || e === 'mssql';
const hasDatabases = (e: Engine) => e !== 'sqlite';
/** Documents in collections, not rows in tables. */
export const isMongo = (e: Engine | undefined) => e === 'mongodb';
/** Keys and values, in numbered databases. */
export const isRedis = (e: Engine | undefined) => e === 'redis';

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
  /** Redis: the keys found so far in each database node (by its tree key), and the SCAN to go on. */
  redisKeys: new Map<string, { keys: RedisKey[]; cursor: string; pattern: string }>(),
  /** Redis: the pattern each database's keys are filtered by (`*` when unset). */
  redisFilters: new Map<string, string>(),
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

export async function saveConnection(config: ConnectionConfig, password: string | null, sentinelPassword: string | null = null) {
  const index = state.connections.findIndex((c) => c.id === config.id);
  if (index >= 0) state.connections[index] = config;
  else state.connections.push(config);
  state.connections.sort((a, b) => a.name.localeCompare(b.name));
  if (config.savePassword && password !== null) await ctx.secrets.set(`password:${config.id}`, password || null);
  if (!config.savePassword) await ctx.secrets.delete(`password:${config.id}`);
  if (password !== null) state.passwords.set(config.id, password);
  // The sentinels' own password is always kept (it is not a user's password).
  if (config.useSentinel && sentinelPassword !== null) await ctx.secrets.set(`sentinel:${config.id}`, sentinelPassword || null);
  if (!config.useSentinel) await ctx.secrets.delete(`sentinel:${config.id}`);
  await save();
  // Reconnect with the new settings when it was connected.
  if (statusOf(config.id).status === 'connected') await disconnect(config.id);
  changed();
}

export async function deleteConnection(id: string) {
  await disconnect(id).catch(() => {});
  state.connections = state.connections.filter((c) => c.id !== id);
  await ctx.secrets.delete(`password:${id}`);
  await ctx.secrets.delete(`sentinel:${id}`);
  state.passwords.delete(id);
  await save();
  changed();
}

export async function storedPassword(id: string): Promise<string | null> {
  return state.passwords.get(id) ?? (await ctx.secrets.get(`password:${id}`));
}

export async function storedSentinelPassword(id: string): Promise<string | null> {
  return ctx.secrets.get(`sentinel:${id}`);
}

export function connectParams(config: ConnectionConfig, password: string | null, sentinelPassword: string | null = null): ConnectParams {
  const { engine, host, port, user, database, file, ssl, trustServerCertificate } = config;
  if (engine === 'sqlite') return { engine, file };
  if ((isMongo(engine) || isRedis(engine)) && config.useUrl) return { engine, url: password ?? undefined };
  if (isRedis(engine) && config.useSentinel) {
    return { engine, sentinels: config.sentinels, masterName: config.masterName, sentinelPassword: sentinelPassword ?? undefined, user, password: password ?? undefined, database, ssl, trustServerCertificate };
  }
  return { engine, host, port, user, password: password ?? undefined, database, ssl, trustServerCertificate };
}

/** Whether connecting needs a secret: a password, or the connection string. MongoDB and Redis may have none. */
const needsSecret = (c: ConnectionConfig) => {
  if (c.engine === 'sqlite') return false;
  if (isMongo(c.engine) || isRedis(c.engine)) return !!(c.useUrl || c.user || c.hasPassword);
  return true;
};

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
    const sentinelPassword = config.useSentinel ? await storedSentinelPassword(id) : null;
    const result = await sql.connect(id, connectParams(config, password, sentinelPassword));
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
  for (const key of [...state.redisKeys.keys()]) if (key.startsWith(`${id}/`)) state.redisKeys.delete(key);
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
  // Redis: refreshing a database (or anything above) scans its keys again.
  for (const key of [...state.redisKeys.keys()]) if (key === node.key || key.startsWith(`${node.key}/`)) state.redisKeys.delete(key);
  if (node.kind === 'namespace' || node.kind === 'key' || node.kind === 'more') {
    const db = redisDatabaseKey(node);
    if (db) return refresh({ ...node, key: db, kind: 'database' });
  }
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
      if (isRedis(config.engine)) {
        // The databases holding keys, and the one the connection opens.
        const preferred = Number(config.database || 0);
        const databases = await redis.databases(connectionId);
        return databases
          .filter((d) => d.keys > 0 || d.db === preferred)
          .map((d) => ({ key: `${node.key}/db:${d.db}`, kind: 'database', label: `db${d.db}`, connectionId, database: String(d.db), schema: null, description: `${d.keys.toLocaleString()} key${d.keys === 1 ? '' : 's'}` }));
      }
      if (!hasDatabases(config.engine)) return groups(node, null, null);
      const databases = await sql.listDatabases(connectionId);
      return databases.map((name) => ({ key: `${node.key}/db:${name}`, kind: 'database', label: name, connectionId, database: name, schema: null }));
    }
    case 'database': {
      if (isRedis(config.engine)) {
        if (!state.redisKeys.has(node.key)) await scanMore(node);
        return redisChildren(node, node.key, '');
      }
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
    case 'namespace': {
      const db = redisDatabaseKey(node)!;
      return redisChildren(node, db, node.prefix ?? '');
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

// ------------------------------------------------------------------------------ Redis keys

/** Keys found per batch: SCAN goes on until this many, or the end. */
const REDIS_BATCH = 2000;

/** The tree key of the Redis database node `node` is in (or is). */
export function redisDatabaseKey(node: TreeNode): string | null {
  const match = /^(.*?\/db:\d+)/.exec(node.key);
  return match ? match[1] : null;
}

export const redisFilter = (dbKey: string) => state.redisFilters.get(dbKey) ?? '';
export const redisLoaded = (dbKey: string) => state.redisKeys.get(dbKey);

/** Finds the next batch of keys of database `node` (the first, if none yet). */
async function scanMore(node: TreeNode) {
  const pattern = redisFilter(node.key) || '*';
  const loaded = state.redisKeys.get(node.key);
  const known = new Set((loaded?.keys ?? []).map((k) => k.name));
  const keys = [...(loaded?.keys ?? [])];
  let cursor = loaded?.cursor ?? '0';
  let first = !loaded;
  // SCAN may return a key twice, and few keys per step on a sparse match: go on until a
  // batch is found or the scan is over.
  while (first || (cursor !== '0' && keys.length - (loaded?.keys.length ?? 0) < REDIS_BATCH)) {
    first = false;
    const step = await redis.scan(node.connectionId, Number(node.database), pattern, cursor, 1000);
    for (const k of step.keys) {
      if (!known.has(k.name)) {
        known.add(k.name);
        keys.push(k);
      }
    }
    cursor = step.cursor;
  }
  keys.sort((a, b) => a.name.localeCompare(b.name));
  state.redisKeys.set(node.key, { keys, cursor, pattern });
}

/** The children of `parent` (a database or a namespace of database `dbKey`): the namespaces
 * and keys right under `prefix`, namespaces first; and, at the top, "load more". */
function redisChildren(parent: TreeNode, dbKey: string, prefix: string): TreeNode[] {
  const loaded = state.redisKeys.get(dbKey);
  if (!loaded) return [];
  const namespaces = new Map<string, number>();
  const leaves: TreeNode[] = [];
  for (const k of loaded.keys) {
    if (!k.name.startsWith(prefix)) continue;
    const rest = k.name.slice(prefix.length);
    const colon = rest.indexOf(':');
    if (colon >= 0) {
      const name = rest.slice(0, colon);
      namespaces.set(name, (namespaces.get(name) ?? 0) + 1);
    } else {
      leaves.push({ key: `${dbKey}/key:${k.name}`, kind: 'key', label: rest || '(empty)', connectionId: parent.connectionId, database: parent.database, schema: null, redisKey: k });
    }
  }
  const children: TreeNode[] = [...namespaces.entries()].map(([name, count]) => ({
    key: `${dbKey}/ns:${prefix}${name}:`,
    kind: 'namespace',
    label: name || '(empty)',
    connectionId: parent.connectionId,
    database: parent.database,
    schema: null,
    prefix: `${prefix}${name}:`,
    description: `${count.toLocaleString()}${loaded.cursor !== '0' ? '+' : ''}`,
  }));
  children.push(...leaves);
  if (prefix === '' && loaded.cursor !== '0') {
    children.push({ key: `${dbKey}/more`, kind: 'more', label: 'Load more keys…', connectionId: parent.connectionId, database: parent.database, schema: null, description: `${loaded.keys.length.toLocaleString()} found so far` });
  }
  return children;
}

/** The tree under database `dbKey` again, from the keys found (keeping what is expanded). */
async function redrawRedis(dbKey: string) {
  for (const key of [...state.children.keys()]) if (key === dbKey || key.startsWith(`${dbKey}/`)) state.children.delete(key);
  const nodes = visibleRows().map((r) => r.node);
  const db = nodes.find((n) => n.key === dbKey);
  if (!db) return changed();
  await load(db);
  // Namespaces open before stay open (their children are computed, no round trips).
  for (const key of [...state.expanded].filter((k) => k.startsWith(`${dbKey}/ns:`)).sort((a, b) => a.length - b.length)) {
    const node = visibleRows().find((r) => r.node.key === key)?.node;
    if (node) await load(node);
  }
}

/** "Load more keys…": the next batch of the database's keys. */
export async function loadMoreKeys(node: TreeNode) {
  const dbKey = redisDatabaseKey(node);
  const db = visibleRows().find((r) => r.node.key === dbKey)?.node;
  if (!db) return;
  state.children.set(node.key, 'loading');
  changed();
  try {
    await scanMore(db);
  } catch (e) {
    forge.window.showMessage((e as Error).message, 'error');
  }
  await redrawRedis(db.key);
}

/** Shows only the keys of database `dbKey` that match `pattern` (glob: `user:*`, `*:session`). */
export async function setRedisFilter(dbKey: string, pattern: string) {
  const trimmed = pattern.trim();
  if (trimmed === '' || trimmed === '*') state.redisFilters.delete(dbKey);
  else state.redisFilters.set(dbKey, trimmed);
  state.redisKeys.delete(dbKey);
  if (!state.expanded.has(dbKey)) state.expanded.add(dbKey);
  await redrawRedis(dbKey);
}

/** `text` as a SCAN pattern that matches it literally (glob characters escaped). */
export const globLiteral = (text: string) => text.replace(/[*?[\]\\]/g, (c) => `\\${c}`);

/** Every key under a namespace (on the server, not just the keys found so far). */
export async function namespaceKeys(node: TreeNode): Promise<RedisKey[]> {
  const pattern = `${globLiteral(node.prefix ?? '')}*`;
  const keys = new Map<string, RedisKey>();
  let cursor = '0';
  do {
    const step = await redis.scan(node.connectionId, Number(node.database), pattern, cursor, 1000);
    for (const k of step.keys) keys.set(k.name, k);
    cursor = step.cursor;
  } while (cursor !== '0');
  return [...keys.values()];
}

/** Deletes `keys` of database `db`, in batches; returns how many were deleted. */
export async function deleteRedisKeys(connectionId: string, db: number, keys: RedisKey[]): Promise<number> {
  let deleted = 0;
  for (let i = 0; i < keys.length; i += 500) {
    deleted += (await redis.remove(connectionId, db, keys.slice(i, i + 500))).deleted;
  }
  await reloadRedisKeys(connectionId, db);
  return deleted;
}

/** After a key was created, renamed or deleted: its database's keys again. */
export async function reloadRedisKeys(connectionId: string, db: number) {
  const dbKey = `${connectionId}/db:${db}`;
  if (!state.redisKeys.has(dbKey)) return;
  state.redisKeys.delete(dbKey);
  await redrawRedis(dbKey);
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
