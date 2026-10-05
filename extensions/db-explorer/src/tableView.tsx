// The rows of a table or view, a page at a time. Tables with a primary key can be edited:
// double-click a cell, add rows, delete rows; nothing is written until Save, which applies
// every change in one transaction.
import { useEffect, useRef, useState } from 'react';
import { forge, Button, DataGrid, Input, Spinner, Text, View } from '@forge/api';
import type { GridRowState, MenuItem } from '@forge/api';
import { requestId, sql } from './client';
import type { Cell, Change, ColumnDetail } from './client';
import * as store from './store';
import { openQuery } from './tabs';
import { selectScript } from './dialect';

type Props = { connectionId: string; database: string | null; schema: string | null; table: string; kind: 'table' | 'view' };

/** A row on screen: what the database has, and what was changed here. */
type Row = { original: Cell[] | null; values: Cell[]; edited: Set<number>; deleted: boolean };

const setting = async <T,>(key: string, fallback: T) => (await forge.settings.get<T>(key)) ?? fallback;

export function TableView({ connectionId, database, schema, table, kind }: Props) {
  const [columns, setColumns] = useState<ColumnDetail[]>([]);
  const [primaryKey, setPrimaryKey] = useState<string[]>([]);
  const [rows, setRows] = useState<Row[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [offset, setOffset] = useState(0);
  const [pageSize, setPageSize] = useState(200);
  const [sort, setSort] = useState<{ column: string; desc: boolean } | null>(null);
  const [where, setWhere] = useState('');
  const [appliedWhere, setAppliedWhere] = useState('');
  const [selected, setSelected] = useState<number[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [message, setMessage] = useState<{ text: string; error?: boolean } | null>(null);
  const running = useRef<string | null>(null);

  const ref = { connectionId, database, schema, table };
  const editable = kind === 'table' && primaryKey.length > 0;
  const pending = rows.filter((r) => r.deleted || r.edited.size > 0 || r.original === null).length;

  const load = async (next: { offset?: number; sort?: typeof sort; where?: string } = {}) => {
    const page = await setting('dbExplorer.pageSize', 200);
    setPageSize(page);
    const at = next.offset ?? offset;
    const order = next.sort !== undefined ? next.sort : sort;
    const filter = next.where ?? appliedWhere;
    setBusy('Loading…');
    setMessage(null);
    const id = requestId();
    running.current = id;
    try {
      if (!(await store.ensureConnected(connectionId))) throw new Error('not connected');
      if (columns.length === 0) {
        const described = await sql.describe(ref);
        setColumns(described.columns);
        setPrimaryKey(described.primaryKey);
      }
      const started = Date.now();
      const result = await sql.fetchTable({ ...ref, offset: at, limit: page, orderBy: order ? [order] : undefined, where: filter || undefined, requestId: id });
      setRows(result.rows.map((values) => ({ original: values, values: [...values], edited: new Set(), deleted: false })));
      setTotal(result.total);
      setOffset(at);
      setSort(order);
      setAppliedWhere(filter);
      setSelected([]);
      setMessage({ text: `${result.rows.length} rows in ${Date.now() - started} ms` });
    } catch (e) {
      setMessage({ text: (e as Error).message, error: true });
    } finally {
      running.current = null;
      setBusy(null);
    }
  };

  useEffect(() => {
    load();
  }, []);

  /** Pages, filters and sorting reload the rows: unsaved changes would be lost. */
  const leave = async () => {
    if (pending === 0) return true;
    const answer = await forge.window.confirm(`Discard ${pending} unsaved ${pending === 1 ? 'change' : 'changes'}?`, { buttons: ['Discard', 'Cancel'], level: 'warning' });
    return answer === 0;
  };

  const editCell = (row: number, column: number, value: Cell) => {
    setRows((rs) =>
      rs.map((r, i) => {
        if (i !== row) return r;
        const values = [...r.values];
        values[column] = value;
        const edited = new Set(r.edited);
        if (r.original && sameCell(r.original[column], value)) edited.delete(column);
        else edited.add(column);
        return { ...r, values, edited };
      }),
    );
  };

  const addRow = () => {
    setRows((rs) => [...rs, { original: null, values: columns.map(() => null), edited: new Set(), deleted: false }]);
    setSelected([rows.length]);
  };

  const deleteRows = (indexes: number[]) => {
    // New rows just go; saved ones are marked, and deleted on Save.
    setRows((rs) => rs.flatMap((r, i) => (indexes.includes(i) ? (r.original === null ? [] : [{ ...r, deleted: true }]) : [r])));
    setSelected([]);
  };

  const restoreRows = (indexes: number[]) => setRows((rs) => rs.map((r, i) => (indexes.includes(i) ? { ...r, deleted: false } : r)));

  const discard = () => {
    setRows((rs) => rs.filter((r) => r.original !== null).map((r) => ({ original: r.original, values: [...r.original!], edited: new Set(), deleted: false })));
  };

  const save = async () => {
    const changes: Change[] = [];
    const keyOf = (r: Row) => Object.fromEntries(primaryKey.map((k) => [k, r.original![columns.findIndex((c) => c.name === k)]]));
    for (const r of rows) {
      if (r.original === null) {
        if (r.deleted) continue;
        // Columns left NULL are left out, so the database fills in defaults (ids, timestamps).
        const values = Object.fromEntries(columns.map((c, i) => [c.name, r.values[i]] as const).filter(([, v]) => v !== null));
        changes.push({ kind: 'insert', values });
      } else if (r.deleted) {
        changes.push({ kind: 'delete', key: keyOf(r) });
      } else if (r.edited.size > 0) {
        changes.push({ kind: 'update', key: keyOf(r), values: Object.fromEntries([...r.edited].map((i) => [columns[i].name, r.values[i]])) });
      }
    }
    if (changes.length === 0) return;
    if (await setting('dbExplorer.confirmChanges', true)) {
      const counts = (['update', 'insert', 'delete'] as const).map((k) => [k, changes.filter((c) => c.kind === k).length] as const).filter(([, n]) => n > 0);
      const summary = counts.map(([k, n]) => `${n} ${k === 'update' ? 'updated' : k === 'insert' ? 'new' : 'deleted'}`).join(', ');
      const answer = await forge.window.confirm(`Save changes to ${table}?`, { detail: `${summary}. They are applied in one transaction.`, buttons: ['Save', 'Cancel'], level: counts.some(([k]) => k === 'delete') ? 'warning' : 'info' });
      if (answer !== 0) return;
    }
    setBusy('Saving…');
    try {
      const result = await sql.applyChanges({ ...ref, changes });
      setBusy(null);
      await load();
      setMessage({ text: `Saved ${result.applied} ${result.applied === 1 ? 'change' : 'changes'}.` });
    } catch (e) {
      setBusy(null);
      setMessage({ text: (e as Error).message, error: true });
    }
  };

  const page = async (to: number) => {
    if (await leave()) load({ offset: Math.max(0, to) });
  };

  const onSort = async (column: string) => {
    if (!(await leave())) return;
    const next = sort?.column !== column ? { column, desc: false } : !sort.desc ? { column, desc: true } : null;
    load({ sort: next, offset: 0 });
  };

  const applyFilter = async (text: string) => {
    if (await leave()) load({ where: text.trim(), offset: 0 });
  };

  const menu: MenuItem[] = [
    ...(editable
      ? ([
          { id: 'null', label: 'Set to NULL' },
          { id: 'delete', label: 'Delete Rows', icon: 'trash', danger: true },
          { id: 'restore', label: 'Restore Rows', icon: 'undo' },
          { separator: true },
        ] as MenuItem[])
      : []),
    { id: 'copy-value', label: 'Copy Value', icon: 'copy' },
    { id: 'copy-rows', label: 'Copy Rows', icon: 'copy' },
  ];

  const onMenu = (e: { id: string; row: number; column: number; rows: number[] }) => {
    switch (e.id) {
      case 'null':
        e.rows.forEach((r) => editCell(r, e.column, null));
        break;
      case 'delete':
        deleteRows(e.rows);
        break;
      case 'restore':
        restoreRows(e.rows);
        break;
      case 'copy-value':
        forge.clipboard.writeText(text(rows[e.row]?.values[e.column] ?? null));
        break;
      case 'copy-rows':
        forge.clipboard.writeText(e.rows.map((r) => rows[r].values.map(text).join('\t')).join('\n'));
        break;
    }
  };

  const rowStates: Record<number, GridRowState> = {};
  const editedCells: string[] = [];
  rows.forEach((r, i) => {
    if (r.original === null) rowStates[i] = 'new';
    else if (r.deleted) rowStates[i] = 'deleted';
    else if (r.edited.size > 0) rowStates[i] = 'modified';
    r.edited.forEach((c) => editedCells.push(`${i}:${c}`));
  });

  const config = store.connection(connectionId);
  const end = offset + rows.filter((r) => r.original !== null).length;
  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 4, paddingX: 8, paddingY: 4, borderSide: 'bottom' }}>
        <Button icon="rotate_cw" variant="ghost" tooltip="Reload" onClick={async () => (await leave()) && load()} />
        {editable && (
          <>
            <Button icon="square_plus" variant="ghost" tooltip="Add Row" onClick={addRow} />
            <Button icon="square_minus" variant="ghost" tooltip="Delete Selected Rows" disabled={selected.length === 0} onClick={() => deleteRows(selected)} />
            <Button label={pending ? `Save ${pending}` : 'Save'} icon="check" variant={pending ? 'filled' : 'ghost'} disabled={pending === 0 || !!busy} onClick={save} />
            <Button label="Discard" icon="undo" variant="ghost" disabled={pending === 0} onClick={discard} />
          </>
        )}
        <View style={{ grow: true, direction: 'row', gap: 4, paddingX: 8 }}>
          <Text style={{ color: 'muted', size: 'sm', mono: true }}>WHERE</Text>
          <View style={{ grow: true }}>
            <Input value={where} placeholder="id > 100 AND name LIKE 'A%'" onChange={setWhere} onSubmit={applyFilter} />
          </View>
        </View>
        <Button icon="file_code" variant="ghost" tooltip="New Query with SELECT" onClick={() => config && openQuery(connectionId, database, selectScript(config.engine, schema, table))} />
      </View>

      {!editable && kind === 'table' && columns.length > 0 && (
        <View style={{ paddingX: 10, paddingY: 4, background: 'surface' }}>
          <Text style={{ size: 'sm', color: 'warning' }}>This table has no primary key, so its rows can't be edited here. Use a query instead.</Text>
        </View>
      )}

      <DataGrid
        style={{ grow: true }}
        columns={columns.map((c) => ({ name: c.name, type: c.type, primaryKey: c.primaryKey }))}
        rows={rows.map((r) => r.values)}
        rowOffset={offset}
        selectedRows={selected}
        rowStates={rowStates}
        editedCells={editedCells}
        editable={editable}
        sort={sort}
        emptyText={busy ? 'Loading…' : appliedWhere ? 'No rows match the filter' : 'The table is empty'}
        onSelect={setSelected}
        onCellEdit={(e) => editCell(e.row, e.column, e.value)}
        onSort={onSort}
        onDeleteRows={(rs) => editable && deleteRows(rs)}
        contextMenu={menu}
        onContextMenu={onMenu}
      />

      <View style={{ direction: 'row', gap: 8, paddingX: 8, paddingY: 4, borderSide: 'top' }}>
        <Button icon="chevron_left" variant="ghost" tooltip="Previous Page" disabled={offset === 0 || !!busy} onClick={() => page(offset - pageSize)} />
        <Text style={{ size: 'sm', color: 'muted' }}>{rows.length ? `${offset + 1}–${end}` : '0'}{total !== null ? ` of ${total}` : ''}</Text>
        <Button icon="chevron_right" variant="ghost" tooltip="Next Page" disabled={(total !== null ? end >= total : rows.length < pageSize) || !!busy} onClick={() => page(offset + pageSize)} />
        {busy && <Spinner />}
        {busy && running.current && <Button label="Cancel" variant="ghost" onClick={() => running.current && sql.cancel(running.current)} />}
        <View style={{ grow: true }}>
          <Text style={{ size: 'sm', color: message?.error ? 'error' : 'muted', truncate: true }}>{busy ?? message?.text ?? ''}</Text>
        </View>
        {pending > 0 && <Text style={{ size: 'sm', color: 'warning' }}>{pending} unsaved</Text>}
      </View>
    </View>
  );
}

function sameCell(a: Cell, b: Cell) {
  return a === b || (a !== null && b !== null && String(a) === String(b));
}

function text(c: Cell) {
  return c === null ? 'NULL' : String(c);
}
