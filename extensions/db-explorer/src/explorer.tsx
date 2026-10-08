// The Databases panel: saved connections and, inside them, databases, schemas, tables and
// views (MongoDB: databases and collections; Redis: databases and their keys, split at `:`
// into namespaces, filtered by a pattern). Double-click a table, view, collection or key to
// open it; right-click anything for more.
import { useEffect, useState } from 'react';
import { forge, Button, Input, Scroll, Text, TreeItem, View } from '@forge-ide/api';
import type { MenuItem } from '@forge-ide/api';
import * as store from './store';
import type { TreeNode, VisibleRow } from './store';
import { openConnectionForm, openConsole, openKey, openNewKey, openQuery, openTable, redisKeyIcon } from './tabs';
import { selectScript } from './dialect';

const ICONS: Record<string, string> = { connection: 'server', database: 'database_zap', schema: 'folder', group: 'folder', table: 'table', view: 'eye', collection: 'json', namespace: 'folder', more: 'ellipsis' };

export function Explorer() {
  const state = store.useStore();
  const rows = store.visibleRows();

  if (state.connections.length === 0) {
    return (
      <View style={{ padding: 12, gap: 8 }}>
        <Text style={{ color: 'muted' }}>No connections yet.</Text>
        <Button label="Add Connection…" icon="plus" variant="filled" onClick={() => openConnectionForm(null)} />
        <Text style={{ color: 'muted', size: 'sm' }}>SQL Server, PostgreSQL, MySQL, MariaDB, SQLite, MongoDB and Redis.</Text>
      </View>
    );
  }
  const selected = rows.find((r) => r.node.key === state.selected)?.node ?? null;
  const selectedEngine = selected ? store.connection(selected.connectionId)?.engine : undefined;
  // MongoDB has no query tab (a collection's tab finds and aggregates); Redis has its console.
  const canQuery = !!selected && !store.isMongo(selectedEngine);
  const redisDb = selected && store.isRedis(selectedEngine) ? store.redisDatabaseKey(selected) : null;
  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 2, paddingX: 6, paddingY: 4, borderSide: 'bottom' }}>
        <Text style={{ weight: 'medium', size: 'sm' }}>Connections</Text>
        <View style={{ grow: true }} />
        <Button icon="plus" variant="ghost" tooltip="Add Connection" onClick={() => openConnectionForm(null)} />
        {store.isRedis(selectedEngine) ? (
          <Button icon="terminal" variant="ghost" tooltip="Open Console" onClick={() => selected && openConsole(selected.connectionId, Number(selected.database ?? store.connection(selected.connectionId)?.database ?? 0))} />
        ) : (
          <Button icon="file_code" variant="ghost" tooltip="New Query" disabled={!canQuery} onClick={() => selected && canQuery && openQuery(selected.connectionId, selected.database)} />
        )}
        <Button icon="rotate_cw" variant="ghost" tooltip="Refresh" disabled={!selected} onClick={() => selected && store.refresh(selected)} />
      </View>
      {redisDb && <KeyFilter dbKey={redisDb} />}
      <Scroll style={{ grow: true, padding: 4 }}>
        {rows.map((row) => (
          <Row key={row.node.key} row={row} selected={row.node.key === state.selected} />
        ))}
      </Scroll>
    </View>
  );
}

/** The pattern a Redis database's keys are filtered by (SCAN MATCH: `user:*`, `*:session`). */
function KeyFilter({ dbKey }: { dbKey: string }) {
  const applied = store.redisFilter(dbKey);
  const [draft, setDraft] = useState(applied);
  useEffect(() => setDraft(store.redisFilter(dbKey)), [dbKey]);
  const db = dbKey.slice(dbKey.lastIndexOf(':') + 1);
  return (
    <View style={{ direction: 'row', gap: 4, paddingX: 6, paddingY: 4, borderSide: 'bottom' }}>
      <View style={{ grow: true }}>
        <Input value={draft} placeholder={`Filter keys in db${db}: user:*, *:session…`} onChange={setDraft} onSubmit={(v) => store.setRedisFilter(dbKey, v)} />
      </View>
      {applied && (
        <Button
          icon="close"
          variant="ghost"
          tooltip="Show all keys"
          onClick={() => {
            setDraft('');
            store.setRedisFilter(dbKey, '');
          }}
        />
      )}
    </View>
  );
}

function Row({ row, selected }: { row: VisibleRow; selected: boolean }) {
  const { node, depth, children } = row;
  const status = node.kind === 'connection' ? store.statusOf(node.connectionId) : null;
  const leaf = node.kind === 'object' || node.kind === 'key' || node.kind === 'more';
  const expanded = store.isExpanded(node.key);
  const loading = status?.status === 'connecting' || (expanded && children === 'loading');
  const error = status?.status === 'error' ? status.message : children && typeof children === 'object' && 'error' in children ? children.error : null;
  const config = store.connection(node.connectionId);
  const icon = node.kind === 'object' ? ICONS[node.object!.kind] : node.kind === 'key' ? redisKeyIcon(node.redisKey!.type) : ICONS[node.kind];
  let description: string | undefined = node.description;
  if (node.kind === 'connection' && config) {
    const where = config.engine === 'sqlite' ? 'file' : config.useUrl ? 'connection string' : config.useSentinel ? `sentinel ${config.masterName ?? ''}` : config.host ?? '';
    description = status?.status === 'connected' ? store.engineLabel(config.engine) : `${store.engineLabel(config.engine)} · ${where}`;
  }
  if (node.kind === 'group' && Array.isArray(children)) description = String(children.length);
  if (node.kind === 'key') description = node.redisKey!.type;
  if (node.kind === 'database' && store.isRedis(config?.engine)) {
    const filter = store.redisFilter(node.key);
    if (filter) description = `${description ?? ''} · ${filter}`;
  }
  if (error) description = error;

  const open = () => {
    if (node.kind === 'object') openTable(node);
    if (node.kind === 'key') openKey(node.connectionId, Number(node.database), node.redisKey!.name, node.redisKey!.escaped, node.redisKey!.type);
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
          if (node.kind === 'more') store.loadMoreKeys(node);
          else if (!leaf) store.toggle(node);
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
  const redisConn = store.isRedis(store.connection(node.connectionId)?.engine);
  if (redisConn) {
    switch (node.kind) {
      case 'database':
        return [
          { id: 'new-key', label: 'New Key…', icon: 'plus' },
          { id: 'console', label: 'Open Console', icon: 'terminal' },
          { id: 'refresh', label: 'Refresh', icon: 'rotate_cw' },
        ];
      case 'namespace':
        return [
          { id: 'refresh', label: 'Refresh', icon: 'rotate_cw' },
          { id: 'copy-prefix', label: 'Copy Prefix', icon: 'copy' },
          { separator: true },
          { id: 'delete-namespace', label: 'Delete All Keys Here…', icon: 'trash', danger: true },
        ];
      case 'key':
        return [
          { id: 'open-key', label: 'Open', icon: redisKeyIcon(node.redisKey!.type) },
          { id: 'copy-name', label: 'Copy Name', icon: 'copy' },
          { separator: true },
          { id: 'delete-key', label: 'Delete Key…', icon: 'trash', danger: true },
        ];
      case 'more':
        return [{ id: 'more', label: 'Load More Keys', icon: 'ellipsis' }];
    }
  }
  switch (node.kind) {
    case 'connection': {
      const connected = store.statusOf(node.connectionId).status === 'connected';
      return [
        connected ? { id: 'disconnect', label: 'Disconnect', icon: 'close' } : { id: 'connect', label: 'Connect', icon: 'link' },
        ...(mongodb ? [] : redisConn ? [{ id: 'console', label: 'Open Console', icon: 'terminal' }] : [{ id: 'query', label: 'New Query', icon: 'file_code' }]),
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
      await store.saveConnection(copy, config.savePassword ? await store.storedPassword(config.id) : null, config.useSentinel ? await store.storedSentinelPassword(config.id) : null);
      break;
    }
    case 'console':
      openConsole(node.connectionId, Number(node.database ?? config.database ?? 0));
      break;
    case 'new-key':
      openNewKey(node.connectionId, Number(node.database));
      break;
    case 'open-key':
      openKey(node.connectionId, Number(node.database), node.redisKey!.name, node.redisKey!.escaped, node.redisKey!.type);
      break;
    case 'more':
      await store.loadMoreKeys(node);
      break;
    case 'copy-prefix':
      await forge.clipboard.writeText(node.prefix ?? '');
      break;
    case 'delete-key': {
      const answer = await forge.window.confirm(`Delete the key “${node.redisKey!.name}”?`, { detail: `It is removed from db${node.database}, with its whole value. This can't be undone.`, buttons: ['Delete', 'Cancel'], level: 'warning' });
      if (answer === 0) await store.deleteRedisKeys(node.connectionId, Number(node.database), [node.redisKey!]);
      break;
    }
    case 'delete-namespace': {
      const keys = await store.namespaceKeys(node);
      if (!keys.length) break;
      const answer = await forge.window.confirm(`Delete the ${keys.length.toLocaleString()} keys under “${node.prefix}”?`, { detail: `Every key starting with ${node.prefix} in db${node.database}, not just the ones shown. This can't be undone.`, buttons: [`Delete ${keys.length.toLocaleString()} Keys`, 'Cancel'], level: 'warning' });
      if (answer === 0) {
        const deleted = await store.deleteRedisKeys(node.connectionId, Number(node.database), keys);
        forge.window.showMessage(`Deleted ${deleted.toLocaleString()} keys.`, 'info');
      }
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
      await forge.clipboard.writeText(node.kind === 'key' ? node.redisKey!.name : node.label);
      break;
    case 'copy-select':
      await forge.clipboard.writeText(selectScript(config.engine, node.schema, node.label));
      break;
  }
}
