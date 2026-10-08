// The Database Explorer's tools for agents: the connections, their schema, and queries.
// Agents work with the databases the user set up here, without credentials of their own.
// Reading the connections and the schema runs at once; a query may change data, so the user
// says yes to each one in the agent's thread.
import { forge } from '@forge-ide/api';
import type { Disposable } from '@forge-ide/api';
import { sql } from './client';
import type { Cell, ResultSet } from './client';
import * as store from './store';
import type { ConnectionConfig } from './store';

const MAX_ROWS = 200;
const SQL_ENGINES = new Set(['postgres', 'mysql', 'mariadb', 'sqlite', 'mssql']);

/** A connection by its name (or id), ignoring case. */
function find(name: string): ConnectionConfig {
  const wanted = name.trim().toLowerCase();
  const found = store.connections().find((c) => c.name.toLowerCase() === wanted || c.id.toLowerCase() === wanted);
  if (!found) {
    const names = store.connections().map((c) => c.name).join(', ') || 'none';
    throw new Error(`No connection is called "${name}". The connections are: ${names}.`);
  }
  if (!SQL_ENGINES.has(found.engine)) throw new Error(`${found.name} is a ${found.engine} connection; these tools work with SQL databases.`);
  return found;
}

async function connected(config: ConnectionConfig) {
  if (!(await store.ensureConnected(config.id))) {
    throw new Error(`Couldn't connect to ${config.name}: connect to it in the Database Explorer (it may need its password), then try again.`);
  }
}

const show = (cell: Cell) => (cell === null ? 'NULL' : String(cell).replace(/\n/g, ' '));

/** A result set as a Markdown table. */
export function table(result: ResultSet): string {
  if (result.columns.length === 0) return result.rowsAffected !== null ? `${result.rowsAffected} rows affected.` : 'Done.';
  const head = `| ${result.columns.map((c) => c.name).join(' | ')} |\n| ${result.columns.map(() => '---').join(' | ')} |`;
  const rows = result.rows.map((row) => `| ${row.map(show).join(' | ')} |`).join('\n');
  const more = result.truncated ? `\n(only the first ${result.rows.length} rows)` : '';
  return `${head}${rows ? '\n' + rows : ''}${more}`;
}

export function registerAgentTools(): Disposable[] {
  return [
    forge.agents.registerTool({
      name: 'connections',
      title: 'Database connections',
      description: "The databases the user connected in the Database Explorer: each connection's name, engine, default database and whether it is connected.",
      readOnly: true,
      run: () => {
        const list = store.connections();
        if (list.length === 0) return 'There are no connections: the user adds them in the Database Explorer.';
        return list
          .map((c) => {
            const status = store.statusOf(c.id).status;
            return `- ${c.name}: ${c.engine}${c.database ? `, database ${c.database}` : ''}${c.file ? `, file ${c.file}` : ''} (${status})`;
          })
          .join('\n');
      },
    }),
    forge.agents.registerTool({
      name: 'schema',
      title: 'Database schema',
      description: 'The tables and views of a SQL connection (in a database and schema if given), or, with `table`, that table\'s columns: name, type, nullability, default and primary key.',
      readOnly: true,
      inputSchema: {
        type: 'object',
        properties: {
          connection: { type: 'string', description: 'The connection\'s name, as `connections` lists it.' },
          database: { type: 'string' },
          schema: { type: 'string' },
          table: { type: 'string', description: 'A table or view to describe.' },
        },
        required: ['connection'],
      },
      run: async (args: { connection: string; database?: string; schema?: string; table?: string }) => {
        const config = find(args.connection);
        await connected(config);
        if (args.table) {
          const { columns } = await sql.describe({ connectionId: config.id, database: args.database, schema: args.schema, table: args.table });
          return columns
            .map((c) => `- ${c.name} ${c.type}${c.nullable ? '' : ' NOT NULL'}${c.primaryKey ? ' PRIMARY KEY' : ''}${c.autoIncrement ? ' (auto)' : ''}${c.default !== null ? ` DEFAULT ${c.default}` : ''}`)
            .join('\n');
        }
        const objects = await sql.listObjects(config.id, args.database, args.schema);
        if (objects.length === 0) return 'No tables or views there.';
        return objects.map((o) => `- ${o.schema ? `${o.schema}.` : ''}${o.name} (${o.kind})`).join('\n');
      },
    }),
    forge.agents.registerTool({
      name: 'query',
      title: 'Run a SQL query',
      description: `Runs SQL on a connection and returns the results as tables (at most ${MAX_ROWS} rows each). It can change data, so the user approves each query.`,
      inputSchema: {
        type: 'object',
        properties: {
          connection: { type: 'string', description: 'The connection\'s name, as `connections` lists it.' },
          sql: { type: 'string', description: 'The statement(s) to run.' },
          database: { type: 'string', description: 'The database to run it in, if not the connection\'s.' },
        },
        required: ['connection', 'sql'],
      },
      run: async (args: { connection: string; sql: string; database?: string }) => {
        const config = find(args.connection);
        await connected(config);
        const { resultSets, elapsedMs } = await sql.query(config.id, args.sql, { database: args.database, maxRows: MAX_ROWS });
        const results = resultSets.map(table).join('\n\n') || 'Done.';
        return `${results}\n\n(${elapsedMs} ms)`;
      },
    }),
  ];
}
