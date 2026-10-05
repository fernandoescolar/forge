// Talks to the forge-sql sidecar (sidecar/README.md): JSON requests, one per line, on its
// input; responses, one per line, on its output. Started on first use and again if it dies.
import { forge } from '@forge/api';
import type { ChildProcess } from '@forge/api';

export type Engine = 'postgres' | 'mysql' | 'mariadb' | 'sqlite' | 'mssql';
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
};

export type ColumnInfo = { name: string; type: string };
export type ResultSet = { columns: ColumnInfo[]; rows: Cell[][]; truncated: boolean; rowsAffected: number | null };
export type DbObject = { name: string; schema: string | null; kind: 'table' | 'view' };
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
