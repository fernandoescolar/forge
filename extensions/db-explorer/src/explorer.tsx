// The Databases panel: saved connections and, inside them, databases, schemas, tables and
// views (MongoDB: databases and collections). Double-click a table, view or collection to
// open it; right-click anything for more.
import { forge, Button, Scroll, Text, TreeItem, View } from '@forge/api';
import type { MenuItem } from '@forge/api';
import * as store from './store';
import type { TreeNode, VisibleRow } from './store';
import { openConnectionForm, openQuery, openTable } from './tabs';
import { selectScript } from './dialect';

const ICONS: Record<string, string> = { connection: 'server', database: 'database_zap', schema: 'folder', group: 'folder', table: 'table', view: 'eye', collection: 'json' };

export function Explorer() {
  const state = store.useStore();
  const rows = store.visibleRows();

  if (state.connections.length === 0) {
    return (
      <View style={{ padding: 12, gap: 8 }}>
        <Text style={{ color: 'muted' }}>No connections yet.</Text>
        <Button label="Add Connection…" icon="plus" variant="filled" onClick={() => openConnectionForm(null)} />
        <Text style={{ color: 'muted', size: 'sm' }}>SQL Server, PostgreSQL, MySQL, MariaDB, SQLite and MongoDB.</Text>
      </View>
    );
  }
  const selected = rows.find((r) => r.node.key === state.selected)?.node ?? null;
  // MongoDB has no query tab: a collection's tab finds and aggregates.
  const canQuery = !!selected && !store.isMongo(store.connection(selected.connectionId)?.engine);
  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 2, paddingX: 6, paddingY: 4, borderSide: 'bottom' }}>
        <Text style={{ weight: 'medium', size: 'sm' }}>Connections</Text>
        <View style={{ grow: true }} />
        <Button icon="plus" variant="ghost" tooltip="Add Connection" onClick={() => openConnectionForm(null)} />
        <Button icon="file_code" variant="ghost" tooltip="New Query" disabled={!canQuery} onClick={() => selected && canQuery && openQuery(selected.connectionId, selected.database)} />
        <Button icon="rotate_cw" variant="ghost" tooltip="Refresh" disabled={!selected} onClick={() => selected && store.refresh(selected)} />
      </View>
      <Scroll style={{ grow: true, padding: 4 }}>
        {rows.map((row) => (
          <Row key={row.node.key} row={row} selected={row.node.key === state.selected} />
        ))}
      </Scroll>
    </View>
  );
}

function Row({ row, selected }: { row: VisibleRow; selected: boolean }) {
  const { node, depth, children } = row;
  const status = node.kind === 'connection' ? store.statusOf(node.connectionId) : null;
  const leaf = node.kind === 'object';
  const expanded = store.isExpanded(node.key);
  const loading = status?.status === 'connecting' || (expanded && children === 'loading');
  const error = status?.status === 'error' ? status.message : children && typeof children === 'object' && 'error' in children ? children.error : null;
  const config = store.connection(node.connectionId);
  const icon = node.kind === 'object' ? ICONS[node.object!.kind] : ICONS[node.kind];
  let description: string | undefined;
  if (node.kind === 'connection' && config) description = status?.status === 'connected' ? store.engineLabel(config.engine) : `${store.engineLabel(config.engine)} · ${config.engine === 'sqlite' ? 'file' : config.useUrl ? 'connection string' : config.host ?? ''}`;
  if (node.kind === 'group' && Array.isArray(children)) description = String(children.length);
  if (error) description = error;

  const open = () => {
    if (node.kind === 'object') openTable(node);
  };
  return (
    <View>
      <TreeItem
        label={node.label}
        description={description}
        icon={icon}
        iconColor={status?.status === 'connected' ? 'success' : error ? 'error' : 'muted'}
        depth={depth}
        expanded={leaf ? undefined : expanded}
        selected={selected}
        loading={loading}
        onToggle={() => store.toggle(node)}
        onClick={() => {
          store.select(node.key);
          if (!leaf) store.toggle(node);
        }}
        onDoubleClick={open}
        contextMenu={menu(node)}
        onContextMenu={(e) => {
          store.select(node.key);
          act(node, e.id);
        }}
      />
    </View>
  );
}

function menu(node: TreeNode): MenuItem[] {
  const mongodb = store.isMongo(store.connection(node.connectionId)?.engine);
  if (mongodb && node.kind === 'object') {
    return [
      { id: 'open', label: node.object!.kind === 'view' ? 'Open View' : 'Open Collection', icon: 'json' },
      { separator: true },
      { id: 'copy-name', label: 'Copy Name', icon: 'copy' },
    ];
  }
  if (mongodb && node.kind !== 'connection') return [{ id: 'refresh', label: 'Refresh', icon: 'rotate_cw' }];
  switch (node.kind) {
    case 'connection': {
      const connected = store.statusOf(node.connectionId).status === 'connected';
      return [
        connected ? { id: 'disconnect', label: 'Disconnect', icon: 'close' } : { id: 'connect', label: 'Connect', icon: 'link' },
        ...(mongodb ? [] : [{ id: 'query', label: 'New Query', icon: 'file_code' }]),
        { id: 'refresh', label: 'Refresh', icon: 'rotate_cw' },
        { separator: true },
        { id: 'edit', label: 'Edit Connection…', icon: 'pencil' },
        { id: 'duplicate', label: 'Duplicate', icon: 'copy' },
        { id: 'delete', label: 'Delete Connection…', icon: 'trash', danger: true },
      ];
    }
    case 'object':
      return [
        { id: 'open', label: node.object!.kind === 'view' ? 'Open View' : 'Open Table', icon: 'table' },
        { id: 'select', label: 'New Query with SELECT', icon: 'file_code' },
        { separator: true },
        { id: 'copy-name', label: 'Copy Name', icon: 'copy' },
        { id: 'copy-select', label: 'Copy SELECT Statement', icon: 'copy' },
      ];
    default:
      return [
        { id: 'query', label: 'New Query', icon: 'file_code' },
        { id: 'refresh', label: 'Refresh', icon: 'rotate_cw' },
      ];
  }
}

async function act(node: TreeNode, id: string) {
  const config = store.connection(node.connectionId);
  if (!config) return;
  switch (id) {
    case 'connect':
      if (!store.isExpanded(node.key)) await store.toggle(node);
      else await store.ensureConnected(node.connectionId);
      break;
    case 'disconnect':
      await store.disconnect(node.connectionId);
      break;
    case 'refresh':
      await store.refresh(node);
      break;
    case 'query':
      openQuery(node.connectionId, node.database);
      break;
    case 'edit':
      openConnectionForm(config.id);
      break;
    case 'duplicate': {
      const copy = { ...config, id: store.newId(), name: `${config.name} copy` };
      await store.saveConnection(copy, config.savePassword ? await store.storedPassword(config.id) : null);
      break;
    }
    case 'delete': {
      const answer = await forge.window.confirm(`Delete the connection “${config.name}”?`, { detail: 'Its saved password is removed from the keychain. The database is not touched.', buttons: ['Delete', 'Cancel'], level: 'warning' });
      if (answer === 0) await store.deleteConnection(config.id);
      break;
    }
    case 'open':
      openTable(node);
      break;
    case 'select':
      openQuery(node.connectionId, node.database, selectScript(config.engine, node.schema, node.label));
      break;
    case 'copy-name':
      await forge.clipboard.writeText(node.label);
      break;
    case 'copy-select':
      await forge.clipboard.writeText(selectScript(config.engine, node.schema, node.label));
      break;
  }
}
