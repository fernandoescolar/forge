// Tabs the explorer opens in the editor area.
import { forge } from '@forge/api';
import { CollectionView } from './collectionView';
import { ConnectionForm } from './connectionForm';
import { ConsoleView } from './consoleView';
import { KeyView, NewKeyView } from './keyView';
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

/** The rows of a table or view, or the documents of a MongoDB collection. */
export function openTable(node: TreeNode) {
  const object = node.object!;
  if (store.isMongo(store.connection(node.connectionId)?.engine)) {
    forge.tabs.open({
      id: `db-explorer.collection:${node.connectionId}:${node.database ?? ''}:${object.name}`,
      title: object.name,
      icon: object.kind === 'view' ? 'eye' : 'json',
      render: () => <CollectionView connectionId={node.connectionId} database={node.database ?? ''} collection={object.name} kind={object.kind === 'view' ? 'view' : 'collection'} />,
    });
    return;
  }
  const kind = object.kind;
  if (kind === 'collection') return;
  const id = `db-explorer.table:${node.connectionId}:${node.database ?? ''}:${node.schema ?? ''}:${object.name}`;
  forge.tabs.open({
    id,
    title: node.schema && node.schema !== 'dbo' && node.schema !== 'public' ? `${node.schema}.${object.name}` : object.name,
    icon: object.kind === 'view' ? 'eye' : 'table',
    render: () => <TableView connectionId={node.connectionId} database={node.database} schema={node.schema} table={object.name} kind={kind} />,
  });
}

const KEY_ICONS: Record<string, string> = { string: 'text_snippet', hash: 'hash', list: 'list_tree', set: 'box', zset: 'arrow_up', stream: 'clock' };
export const redisKeyIcon = (type: string) => KEY_ICONS[type] ?? 'binary';

/** A Redis key: its value by type, TTL, rename and delete. */
export function openKey(connectionId: string, db: number, name: string, escaped: boolean, type: string) {
  const tab = forge.tabs.open({
    id: `db-explorer.key:${connectionId}:${db}:${name}`,
    title: name,
    icon: redisKeyIcon(type),
    render: () => <KeyView connectionId={connectionId} db={db} keyName={name} keyEscaped={escaped} onRenamed={(to) => tab.update({ title: to })} onDeleted={() => tab.close()} />,
  });
}

/** The form for a new Redis key in database `db`; the key's tab opens once created. */
export function openNewKey(connectionId: string, db: number) {
  const tab = forge.tabs.open({
    id: `db-explorer.newkey:${connectionId}:${db}`,
    title: `New Key · db${db}`,
    icon: 'plus',
    render: () => (
      <NewKeyView
        connectionId={connectionId}
        db={db}
        onCreated={(key, type) => {
          tab.close();
          openKey(connectionId, db, key, false, type);
        }}
      />
    ),
  });
}

let consoles = 0;

/** A Redis console on database `db`. */
export function openConsole(connectionId: string, db: number) {
  const name = store.connection(connectionId)?.name ?? connectionId;
  forge.tabs.open({
    id: `db-explorer.console:${++consoles}`,
    title: `Console ${consoles} · ${name}`,
    icon: 'terminal',
    render: () => <ConsoleView connectionId={connectionId} db={db} />,
  });
}
