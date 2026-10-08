// Talks to the forge-sql sidecar (sidecar/README.md): JSON requests, one per line, on its
// input; responses, one per line, on its output. Started on first use and again if it dies.
import { forge } from '@forge-ide/api';
import type { ChildProcess } from '@forge-ide/api';

export type Engine = 'postgres' | 'mysql' | 'mariadb' | 'sqlite' | 'mssql' | 'mongodb' | 'redis';
export type Cell = string | number | boolean | null;

export type ConnectParams = {
  engine: Engine;
  host?: string;
  port?: number;
  user?: string;
  password?: string;
  database?: string;
  file?: string;
  ssl?: 'disable' | 'prefer' | 'require';
  trustServerCertificate?: boolean;
  /** A whole connection string (MongoDB: `mongodb://…`, `mongodb+srv://…`; Redis: `redis://…`, `rediss://…`); overrides the rest. */
  url?: string;
  /** Redis Sentinel: the sentinels (`host:port`, comma-separated), the master's name, their password. */
  sentinels?: string;
  masterName?: string;
  sentinelPassword?: string;
};

export type ColumnInfo = { name: string; type: string };
export type ResultSet = { columns: ColumnInfo[]; rows: Cell[][]; truncated: boolean; rowsAffected: number | null };
export type DbObject = { name: string; schema: string | null; kind: 'table' | 'view' | 'collection' };
export type ColumnDetail = { name: string; type: string; nullable: boolean; default: string | null; primaryKey: boolean; autoIncrement: boolean };
export type TableRef = { connectionId: string; database?: string | null; schema?: string | null; table: string };
export type Change =
  | { kind: 'update'; key: Record<string, Cell>; values: Record<string, Cell> }
  | { kind: 'delete'; key: Record<string, Cell> }
  | { kind: 'insert'; values: Record<string, Cell> };

export class SqlError extends Error {
  constructor(message: string, readonly code?: string) {
    super(message);
  }
}

type Pending = { resolve: (v: any) => void; reject: (e: Error) => void };

class Client {
  private process: Promise<ChildProcess> | null = null;
  private next = 1;
  private pending = new Map<number, Pending>();
  private stderr = '';

  private start(): Promise<ChildProcess> {
    if (this.process) return this.process;
    this.process = forge.process.sidecar('forge-sql').then((p) => {
      p.onLine((line) => this.receive(line));
      p.onStderr((text) => {
        this.stderr = (this.stderr + text).slice(-4000);
        console.log(`[forge-sql] ${text.trimEnd()}`);
      });
      p.onExit((code) => {
        this.process = null;
        const error = new SqlError(`the database helper stopped (exit code ${code})${this.stderr ? `: ${this.stderr.trim().split('\n').pop()}` : ''}`);
        this.pending.forEach((p) => p.reject(error));
        this.pending.clear();
        connectedListeners.forEach((l) => l());
      });
      return p;
    });
    this.process.catch(() => (this.process = null));
    return this.process;
  }

  private receive(line: string) {
    if (!line.trim()) return;
    let message: { id: number; result?: unknown; error?: { message: string; code?: string } };
    try {
      message = JSON.parse(line);
    } catch {
      console.warn(`[forge-sql] not JSON: ${line.slice(0, 200)}`);
      return;
    }
    const p = this.pending.get(message.id);
    if (!p) return;
    this.pending.delete(message.id);
    if (message.error) p.reject(new SqlError(message.error.message, message.error.code));
    else p.resolve(message.result);
  }

  async request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    const process = await this.start();
    const id = this.next++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      process.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }

  stop() {
    this.process?.then((p) => p.kill()).catch(() => {});
    this.process = null;
  }
}

const client = new Client();
const connectedListeners = new Set<() => void>();

/** Called when the sidecar stops (every connection is gone). */
export function onSidecarExit(listener: () => void) {
  connectedListeners.add(listener);
  return { dispose: () => connectedListeners.delete(listener) };
}

let nextRequest = 1;
/** An id for a query that can be cancelled. */
export const requestId = () => `q${nextRequest++}`;

export const sql = {
  connect: (connectionId: string, params: ConnectParams) => client.request<{ serverVersion: string; defaultDatabase: string | null; engine: Engine }>('connect', { connectionId, ...params }),
  disconnect: (connectionId: string) => client.request<null>('disconnect', { connectionId }),
  listDatabases: (connectionId: string) => client.request<string[]>('listDatabases', { connectionId }),
  listSchemas: (connectionId: string, database?: string | null) => client.request<string[]>('listSchemas', { connectionId, database }),
  listObjects: (connectionId: string, database?: string | null, schema?: string | null) => client.request<DbObject[]>('listObjects', { connectionId, database, schema }),
  describe: (t: TableRef) => client.request<{ columns: ColumnDetail[]; primaryKey: string[] }>('describe', t),
  query: (connectionId: string, text: string, opts: { database?: string | null; maxRows?: number; requestId?: string } = {}) =>
    client.request<{ resultSets: ResultSet[]; elapsedMs: number }>('query', { connectionId, sql: text, ...opts }),
  fetchTable: (t: TableRef & { offset: number; limit: number; orderBy?: { column: string; desc: boolean }[]; where?: string; requestId?: string }) =>
    client.request<{ columns: ColumnInfo[]; rows: Cell[][]; total: number | null }>('fetchTable', t),
  applyChanges: (t: TableRef & { changes: Change[] }) => client.request<{ applied: number }>('applyChanges', t),
  cancel: (id: string) => client.request<{ cancelled: boolean }>('cancel', { requestId: id }),
  stop: () => client.stop(),
};

// ------------------------------------------------------------------------------ MongoDB

/**
 * A document found: as text (JSON with `ObjectId("…")`, `ISODate("…")`… so types survive an
 * edit), its `_id` in the same form (to save or delete it), and its top-level fields in
 * short, for the table.
 */
export type FoundDocument = { id: string | null; text: string; fields: Record<string, Cell> };
export type Documents = { documents: FoundDocument[]; truncated: boolean; total: number | null; elapsedMs: number };
export type CollectionRef = { connectionId: string; database: string; collection: string };

export const mongo = {
  listCollections: (connectionId: string, database: string) => client.request<{ name: string; kind: 'collection' | 'view' }[]>('listCollections', { connectionId, database }),
  listIndexes: (c: CollectionRef) => client.request<{ name: string; keys: string; unique: boolean }[]>('listIndexes', c),
  find: (c: CollectionRef & { filter?: string; sort?: string; projection?: string; skip?: number; limit?: number; requestId?: string }) => client.request<Documents>('find', c),
  aggregate: (c: CollectionRef & { pipeline: string; maxDocs?: number; requestId?: string }) => client.request<Documents>('aggregate', c),
  insert: (c: CollectionRef, document: string) => client.request<{ id: string }>('insertDocument', { ...c, document }),
  replace: (c: CollectionRef, id: string, document: string) => client.request<null>('replaceDocument', { ...c, id, document }),
  remove: (c: CollectionRef, id: string) => client.request<null>('deleteDocument', { ...c, id }),
};

// ------------------------------------------------------------------------------ Redis

/** A key found by SCAN. `escaped`: its name isn't UTF-8 and is shown with `\xNN` escapes. */
export type RedisKey = { name: string; escaped: boolean; type: string };
/**
 * A key and a page of its value: `text` for a string; else `columns` and `rows` (hash: field,
 * value; list: index, value; set: member; zset: member, score; stream: id, fields as JSON),
 * and `next` to pass back (`cursor` for hashes, sets and streams, `offset` for lists and zsets).
 */
export type RedisValue = {
  key: string;
  type: 'string' | 'hash' | 'list' | 'set' | 'zset' | 'stream';
  /** Seconds left; -1 without expiry. */
  ttl: number;
  length: number;
  text: string | null;
  escaped: boolean;
  columns: string[];
  rows: Cell[][];
  escapedRows: boolean[];
  next: string | number | null;
};
export type KeyRef = { connectionId: string; db: number; key: string; keyEscaped?: boolean };
/** One change to a key (`op` and its fields, as the sidecar's `editKey` takes them). */
export type RedisEdit =
  | { op: 'setString'; value: string }
  | { op: 'hashSet'; field: string; value: string }
  | { op: 'hashDelete'; fields: string[] }
  | { op: 'listSet'; index: number; value: string }
  | { op: 'listPush'; value: string; head?: boolean }
  | { op: 'listDelete'; indexes: number[] }
  | { op: 'setAdd'; member: string }
  | { op: 'setDelete'; members: string[] }
  | { op: 'setRename'; member: string; to: string }
  | { op: 'zSetAdd'; member: string; score: number }
  | { op: 'zSetDelete'; members: string[] }
  | { op: 'zSetRename'; member: string; to: string }
  | { op: 'streamAdd'; fields: [string, string][] }
  | { op: 'streamDelete'; ids: string[] }
  | { op: 'create'; type: string; field?: string; value?: string; score?: number };

export const redis = {
  databases: (connectionId: string) => client.request<{ db: number; keys: number }[]>('redisDatabases', { connectionId }),
  scan: (connectionId: string, db: number, pattern: string, cursor = '0', count = 1000, requestId?: string) =>
    client.request<{ cursor: string; keys: RedisKey[] }>('scanKeys', { connectionId, db, pattern, cursor, count, requestId }),
  get: (k: KeyRef & { cursor?: string; offset?: number; limit?: number }) => client.request<RedisValue>('getKey', k),
  /** `escaped`: the texts in `edit` are escaped (the row or value was shown escaped). */
  edit: (k: KeyRef, edit: RedisEdit, escaped = false) => client.request<null>('editKey', { ...k, escaped, ...edit }),
  expire: (k: KeyRef, ttl: number | null) => client.request<null>('expireKey', { ...k, ttl }),
  rename: (k: KeyRef, to: string) => client.request<null>('renameKey', { ...k, to }),
  remove: (connectionId: string, db: number, keys: { name: string; escaped: boolean }[]) =>
    client.request<{ deleted: number }>('deleteKeys', { connectionId, db, keys: keys.map((k) => [k.name, k.escaped]) }),
  command: (connectionId: string, db: number, line: string, requestId?: string) => client.request<{ output: string; elapsedMs: number }>('redisCommand', { connectionId, db, line, requestId }),
};
