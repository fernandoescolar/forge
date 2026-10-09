// A MongoDB collection: find documents (filter, sort, projection, a page at a time) or run
// an aggregation pipeline; the top-level fields in a table, and the selected document as
// text to edit and save whole (replaceOne by _id), insert or delete. Documents are JSON with
// the shell's helpers (ObjectId("…"), ISODate("…"), NumberLong("…")…), so types survive.
import { useEffect, useRef, useState } from 'react';
import { forge, Button, DataGrid, Input, Select, Spinner, Text, View } from '@forge-ide/api';
import type { Cell } from '@forge-ide/api';
import { mongo, requestId, sql } from './client';
import type { CollectionRef, Documents } from './client';
import * as store from './store';

type Props = { connectionId: string; database: string; collection: string; kind: 'collection' | 'view' };
type Mode = 'find' | 'aggregate';
/** What the editor shows: a document found (by index), a new one, or nothing. */
type Editing = { kind: 'existing'; index: number; id: string } | { kind: 'new' } | null;

/** Documents per page to choose from. */
export const PAGE_SIZES = [10, 20, 50, 100, 200, 500, 1000];

/** The page size closest to `wanted` (the Page size setting) among [`PAGE_SIZES`]. */
export const closestPageSize = (wanted: number) => PAGE_SIZES.reduce((best, n) => (Math.abs(n - wanted) < Math.abs(best - wanted) ? n : best));

/** Columns: `_id` first, then the fields in the order they first appear; at most 50. */
export function columnsOf(docs: Documents['documents']): string[] {
  const seen = new Set<string>(['_id']);
  const columns = ['_id'];
  for (const d of docs) {
    for (const key of Object.keys(d.fields)) {
      if (!seen.has(key) && columns.length < 50) {
        seen.add(key);
        columns.push(key);
      }
    }
  }
  return docs.some((d) => '_id' in d.fields) ? columns : columns.slice(1);
}

export function CollectionView({ connectionId, database, collection, kind }: Props) {
  const ref: CollectionRef = { connectionId, database, collection };
  const readOnly = kind === 'view';
  const [mode, setMode] = useState<Mode>('find');
  const [filter, setFilter] = useState('');
  const [sort, setSort] = useState('');
  const [projection, setProjection] = useState('');
  const [pipeline, setPipeline] = useState('[\n  { "$match": {} }\n]');
  const [skip, setSkip] = useState(0);
  const [pageSize, setPageSize] = useState(200);
  const [result, setResult] = useState<Documents | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing>(null);
  const [text, setText] = useState('');
  const [saving, setSaving] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  /** The document to select again after a reload (its _id). */
  const keep = useRef<string | null>(null);
  /** The latest search: an earlier one that answers late is ignored. */
  const latest = useRef<string | null>(null);
  /** What the boxes hold now: a reload started earlier (after a save) searches with it. */
  const inputs = useRef({ mode, filter, sort, projection, pipeline, pageSize });
  inputs.current = { mode, filter, sort, projection, pipeline, pageSize };

  const docs = result?.documents ?? [];
  const current = editing?.kind === 'existing' ? docs[editing.index] : null;
  const dirty = editing?.kind === 'new' ? text.trim() !== '' && text.trim() !== '{}' : !!current && text !== current.text;

  const run = async (opts: { skip?: number; mode?: Mode; size?: number } = {}) => {
    // A new search replaces the one still running (instead of being dropped).
    if (latest.current) sql.cancel(latest.current).catch(() => {});
    const id = requestId();
    latest.current = id;
    const { filter, sort, projection, pipeline, pageSize } = inputs.current;
    const runMode = opts.mode ?? inputs.current.mode;
    const from = opts.skip ?? 0;
    setRunning(id);
    setError(null);
    try {
      if (!(await store.ensureConnected(connectionId))) throw new Error('Not connected.');
      const size = opts.size ?? pageSize;
      const found =
        runMode === 'find'
          ? await mongo.find({ ...ref, filter, sort, projection, skip: from, limit: size, requestId: id })
          : await mongo.aggregate({ ...ref, pipeline, maxDocs: (await forge.settings.get<number>('dbExplorer.maxRows')) ?? 1000, requestId: id });
      if (latest.current !== id) return;
      setResult(found);
      setSkip(runMode === 'find' ? from : 0);
      // Keep the selected document selected, if it is still there.
      const index = keep.current ? found.documents.findIndex((d) => d.id === keep.current) : -1;
      if (index >= 0) select(index, found);
      else if (editing?.kind !== 'new') {
        setEditing(null);
        setText('');
      }
    } catch (e) {
      if (latest.current === id) setError((e as Error).message);
    } finally {
      if (latest.current === id) {
        latest.current = null;
        setRunning(null);
      }
    }
  };

  // The first page right away, as many documents as the Page size setting says.
  useEffect(() => {
    forge.settings.get<number>('dbExplorer.pageSize').then((wanted) => {
      const size = closestPageSize(wanted ?? 200);
      setPageSize(size);
      run({ size });
    });
  }, [connectionId, database, collection]);

  const select = (index: number, found = result) => {
    const doc = found?.documents[index];
    if (!doc) return;
    setEditError(null);
    setText(doc.text);
    if (doc.id) {
      keep.current = doc.id;
      setEditing({ kind: 'existing', index, id: doc.id });
    } else {
      // An aggregation result without _id can be read, not saved.
      keep.current = null;
      setEditing(null);
      setText(doc.text);
    }
  };

  const insertNew = () => {
    keep.current = null;
    setEditError(null);
    setEditing({ kind: 'new' });
    setText('{\n  \n}');
  };

  const confirm = async (message: string, detail: string, button: string) => {
    if (!((await forge.settings.get<boolean>('dbExplorer.confirmChanges')) ?? true)) return true;
    return (await forge.window.confirm(message, { detail, buttons: [button, 'Cancel'], level: 'warning' })) === 0;
  };

  const save = async () => {
    if (!editing || saving) return;
    setSaving(true);
    setEditError(null);
    try {
      if (editing.kind === 'new') {
        const { id } = await mongo.insert(ref, text);
        keep.current = id;
        forge.window.showMessage(`Inserted ${id}.`, 'info');
        setEditing(null);
      } else {
        if (!(await confirm(`Save the document ${editing.id}?`, 'It replaces the document in the database.', 'Save'))) return;
        await mongo.replace(ref, editing.id, text);
        keep.current = editing.id;
      }
      await run({ skip });
    } catch (e) {
      setEditError((e as Error).message);
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    if (editing?.kind !== 'existing') return;
    if (!(await confirm(`Delete the document ${editing.id}?`, `It is removed from ${collection}. This can't be undone.`, 'Delete'))) return;
    try {
      await mongo.remove(ref, editing.id);
      keep.current = null;
      setEditing(null);
      setText('');
      await run({ skip });
    } catch (e) {
      setEditError((e as Error).message);
    }
  };

  const columns = columnsOf(docs);
  const rows: Cell[][] = docs.map((d) => columns.map((c) => (c in d.fields ? d.fields[c] : '')));
  const total = result?.total ?? null;
  const shown =
    mode === 'find' && result
      ? docs.length === 0
        ? 'No documents'
        : `${skip + 1}–${skip + docs.length}${total !== null ? ` of ${total.toLocaleString()}` : ''}`
      : result
        ? `${docs.length}${result.truncated ? '+' : ''} documents`
        : '';
  const hasNext = mode === 'find' && (total !== null ? skip + docs.length < total : docs.length === pageSize);

  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 4, borderSide: 'bottom' }}>
        <Button label="Find" variant="ghost" selected={mode === 'find'} onClick={() => setMode('find')} />
        <Button label="Aggregate" variant="ghost" selected={mode === 'aggregate'} onClick={() => setMode('aggregate')} />
        {running ? (
          <Button label="Cancel" icon="stop" variant="filled" onClick={() => sql.cancel(running)} />
        ) : (
          <Button label="Run" icon="play_filled" variant="filled" tooltip={mode === 'find' ? 'Find (Enter in a field)' : `Run the pipeline (${forge.shortcut('secondary-enter')})`} onClick={() => run()} />
        )}
        {running && <Spinner />}
        <View style={{ grow: true }} />
        <Text style={{ size: 'sm', color: 'muted' }}>{shown}{result ? ` · ${result.elapsedMs} ms` : ''}</Text>
        {mode === 'find' && (
          <>
            <Button icon="chevron_left" variant="ghost" tooltip="Previous page" disabled={!!running || skip === 0} onClick={() => run({ skip: Math.max(0, skip - pageSize) })} />
            <Button icon="chevron_right" variant="ghost" tooltip="Next page" disabled={!!running || !hasNext} onClick={() => run({ skip: skip + pageSize })} />
            <Select
              value={pageSize}
              options={PAGE_SIZES.map((n) => ({ value: n, label: `${n} per page` }))}
              disabled={!!running}
              onChange={(size) => {
                setPageSize(size);
                run({ size });
              }}
            />
          </>
        )}
        {!readOnly && <Button label="Insert Document" icon="plus" variant="ghost" onClick={insertNew} />}
      </View>

      {mode === 'find' ? (
        <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 6, borderSide: 'bottom' }}>
          <View style={{ grow: true }}>
            <Input value={filter} language="JSON" placeholder='Filter: { "status": "active", "_id": ObjectId("…") }' onChange={setFilter} onSubmit={() => run()} autoFocus />
          </View>
          <View style={{ width: 200 }}>
            <Input value={sort} language="JSON" placeholder='Sort: { "createdAt": -1 }' onChange={setSort} onSubmit={() => run()} />
          </View>
          <View style={{ width: 200 }}>
            <Input value={projection} language="JSON" placeholder='Fields: { "name": 1 }' onChange={setProjection} onSubmit={() => run()} />
          </View>
        </View>
      ) : (
        <View style={{ padding: 8, shrink: false, borderSide: 'bottom' }}>
          <Input value={pipeline} multiline language="JSON" placeholder='[{ "$match": {} }, { "$group": { … } }]' onChange={setPipeline} onSubmit={() => run({ mode: 'aggregate' })} />
        </View>
      )}

      {error ? (
        <View style={{ padding: 10 }}>
          <Text style={{ color: 'error', mono: true }} selectable>
            {error}
          </Text>
        </View>
      ) : (
        // `stretch`: a row centres its children, and the table has no height of its own.
        <View style={{ direction: 'row', grow: true, align: 'stretch' }}>
          <DataGrid
            style={{ grow: true }}
            columns={columns.map((name) => ({ name, primaryKey: name === '_id' }))}
            rows={rows}
            rowOffset={mode === 'find' ? skip : 0}
            selectedRows={editing?.kind === 'existing' ? [editing.index] : []}
            emptyText={running ? 'Loading…' : 'No documents'}
            onSelect={(selected) => selected.length === 1 && select(selected[0])}
            onRowActivate={(row) => select(row)}
            contextMenu={[
              { id: 'copy', label: 'Copy Document', icon: 'copy' },
              { id: 'copy-id', label: 'Copy _id', icon: 'copy' },
            ]}
            onContextMenu={(e) => {
              const doc = docs[e.row];
              if (!doc) return;
              forge.clipboard.writeText(e.id === 'copy-id' ? doc.id ?? '' : e.rows.map((r) => docs[r]?.text ?? '').join('\n'));
            }}
          />
          {(editing || text) && (
            <View style={{ width: 460, borderSide: 'left', shrink: false }}>
              <View style={{ direction: 'row', gap: 4, paddingX: 8, paddingY: 4, borderSide: 'bottom' }}>
                <Text style={{ size: 'sm', weight: 'medium', truncate: true }}>{editing?.kind === 'new' ? 'New document' : editing?.kind === 'existing' ? editing.id : 'Document'}</Text>
                <View style={{ grow: true }} />
                {!readOnly && editing && <Button label={editing.kind === 'new' ? 'Insert' : 'Save'} icon="check" variant="filled" disabled={!dirty || saving} tooltip={forge.shortcut('secondary-enter')} onClick={save} />}
                {editing?.kind === 'existing' && dirty && <Button icon="undo" variant="ghost" tooltip="Revert" onClick={() => current && setText(current.text)} />}
                {!readOnly && editing?.kind === 'existing' && <Button icon="trash" variant="ghost" tooltip="Delete Document" onClick={remove} />}
                <Button
                  icon="close"
                  variant="ghost"
                  tooltip="Close"
                  onClick={() => {
                    keep.current = null;
                    setEditing(null);
                    setText('');
                  }}
                />
              </View>
              {editError && (
                <View style={{ paddingX: 8, paddingY: 4 }}>
                  <Text style={{ color: 'error', size: 'sm' }} selectable>
                    {editError}
                  </Text>
                </View>
              )}
              <View style={{ grow: true, padding: 8 }}>
                <Input value={text} multiline language="JSON" onChange={readOnly || !editing ? undefined : setText} onSubmit={() => !readOnly && dirty && save()} />
              </View>
            </View>
          )}
        </View>
      )}
    </View>
  );
}
