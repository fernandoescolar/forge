// Tabs the explorer opens in the editor area.
import { forge } from '@forge/api';
import { ConnectionForm } from './connectionForm';
import { QueryView } from './queryView';
import { TableView } from './tableView';
import * as store from './store';
import type { TreeNode } from './store';

/** The form to add a connection (`id` null) or edit one. */
export function openConnectionForm(id: string | null, message?: string) {
  const tabId = `db-explorer.connection:${id ?? 'new'}`;
  const tab = forge.tabs.open({
    id: tabId,
    title: id ? `Connection: ${store.connection(id)?.name ?? id}` : 'New Connection',
    icon: 'server',
    render: () => <ConnectionForm id={id} message={message} onDone={() => tab.close()} />,
  });
}

let queries = 0;

/** A query editor on a connection (and database), optionally with some SQL in it. */
export function openQuery(connectionId: string, database: string | null, text = '') {
  const name = store.connection(connectionId)?.name ?? connectionId;
  const id = `db-explorer.query:${++queries}`;
  forge.tabs.open({
    id,
    title: `Query ${queries} · ${name}`,
    icon: 'file_code',
    render: () => <QueryView connectionId={connectionId} database={database} initialText={text} />,
  });
}

/** The rows of a table or view. */
export function openTable(node: TreeNode) {
  const object = node.object!;
  const id = `db-explorer.table:${node.connectionId}:${node.database ?? ''}:${node.schema ?? ''}:${object.name}`;
  forge.tabs.open({
    id,
    title: node.schema && node.schema !== 'dbo' && node.schema !== 'public' ? `${node.schema}.${object.name}` : object.name,
    icon: object.kind === 'view' ? 'eye' : 'table',
    render: () => <TableView connectionId={node.connectionId} database={node.database} schema={node.schema} table={object.name} kind={object.kind} />,
  });
}
