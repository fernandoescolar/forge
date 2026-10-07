// A Redis key: its type, TTL (set or removed), size, rename and delete; and its value by
// type. A string is text to edit (highlighted as JSON when it is JSON). A hash, list, set,
// sorted set or stream is a table a page at a time: cells edit in place and rows are marked
// for deletion until Save applies them all; a row at the bottom adds an entry. Values that
// aren't UTF-8 show escaped (`\xNN`) and are written back from that form.
import { useEffect, useRef, useState } from 'react';
import { forge, Button, DataGrid, Input, Select, Spinner, Text, View } from '@forge/api';
import type { Cell, GridRowState } from '@forge/api';
import { redis } from './client';
import type { KeyRef, RedisEdit, RedisValue } from './client';
import * as store from './store';

type Props = { connectionId: string; db: number; keyName: string; keyEscaped: boolean; onRenamed?: (to: string) => void; onDeleted?: () => void };

/** A change to a row, kept until Save. */
type Pending = { values: Record<number, string> } | { deleted: true };

const TYPE_LABELS: Record<string, string> = { string: 'String', hash: 'Hash', list: 'List', set: 'Set', zset: 'Sorted set', stream: 'Stream' };

/** "1 h 5 min" for a TTL in seconds. */
export function ttlText(ttl: number): string {
  if (ttl < 0) return 'No expiry';
  const units: [number, string][] = [
    [86400, 'd'],
    [3600, 'h'],
    [60, 'min'],
    [1, 's'],
  ];
  const parts: string[] = [];
  let left = ttl;
  for (const [size, unit] of units) {
    if (left >= size && parts.length < 2) {
      parts.push(`${Math.floor(left / size)} ${unit}`);
      left %= size;
    }
  }
  return parts.length ? parts.join(' ') : '0 s';
}

const looksLikeJson = (text: string) => /^\s*[[{]/.test(text);

/**
 * The edits that a row's pending changes make, for a key of `type`; `row` is the row as
 * loaded (its cells, in the table's columns).
 */
export function editsFor(type: RedisValue['type'], row: Cell[], change: Pending): RedisEdit[] {
  const text = (c: Cell) => (c === null ? '' : String(c));
  if ('deleted' in change) {
    switch (type) {
      case 'hash':
        return [{ op: 'hashDelete', fields: [text(row[0])] }];
      case 'list':
        return [{ op: 'listDelete', indexes: [Number(row[0])] }];
      case 'set':
        return [{ op: 'setDelete', members: [text(row[0])] }];
      case 'zset':
        return [{ op: 'zSetDelete', members: [text(row[0])] }];
      case 'stream':
        return [{ op: 'streamDelete', ids: [text(row[0])] }];
      default:
        return [];
    }
  }
  const v = change.values;
  switch (type) {
    case 'hash': {
      const field = text(row[0]);
      const newField = v[0] ?? field;
      const value = v[1] ?? text(row[1]);
      // A renamed field: the new one, then the old one goes.
      return newField !== field ? [{ op: 'hashSet', field: newField, value }, { op: 'hashDelete', fields: [field] }] : [{ op: 'hashSet', field, value }];
    }
    case 'list':
      return v[1] !== undefined ? [{ op: 'listSet', index: Number(row[0]), value: v[1] }] : [];
    case 'set':
      return v[0] !== undefined && v[0] !== text(row[0]) ? [{ op: 'setRename', member: text(row[0]), to: v[0] }] : [];
    case 'zset': {
      const member = text(row[0]);
      const edits: RedisEdit[] = [];
      if (v[1] !== undefined) {
        const score = Number(v[1]);
        if (!Number.isFinite(score)) throw new Error(`“${v[1]}” is not a score (a number)`);
        edits.push({ op: 'zSetAdd', member, score });
      }
      if (v[0] !== undefined && v[0] !== member) edits.push({ op: 'zSetRename', member, to: v[0] });
      return edits;
    }
    default:
      return [];
  }
}

/** Which columns can be edited in place, by type: list indexes and stream entries can't. */
const editableColumns: Record<string, number[]> = { hash: [0, 1], list: [1], set: [0], zset: [0, 1], stream: [] };

export function KeyView({ connectionId, db, keyName, keyEscaped, onRenamed, onDeleted }: Props) {
  const [name, setName] = useState(keyName);
  const ref: KeyRef = { connectionId, db, key: name, keyEscaped };
  const [value, setValue] = useState<RedisValue | null>(null);
  const [rows, setRows] = useState<Cell[][]>([]);
  const [escapedRows, setEscapedRows] = useState<boolean[]>([]);
  const [next, setNext] = useState<string | number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [text, setText] = useState('');
  const [pending, setPending] = useState<Record<number, Pending>>({});
  const [selected, setSelected] = useState<number[]>([]);
  const [ttlInput, setTtlInput] = useState('');
  const [renaming, setRenaming] = useState<string | null>(null);
  // The row to add: field/member/index, value/score, and where a list item goes.
  const [addA, setAddA] = useState('');
  const [addB, setAddB] = useState('');
  const [listEnd, setListEnd] = useState<'tail' | 'head'>('tail');
  const loadedOnce = useRef(false);

  const limit = 500;

  const load = async (more = false) => {
    setBusy(true);
    setError(null);
    try {
      if (!(await store.ensureConnected(connectionId))) throw new Error('Not connected.');
      const page = await redis.get({
        ...ref,
        limit,
        ...(more && next !== null ? (typeof next === 'number' ? { offset: next } : { cursor: next }) : {}),
      });
      setValue(page);
      setNext(page.next);
      if (more) {
        setRows((r) => [...r, ...page.rows]);
        setEscapedRows((r) => [...r, ...page.escapedRows]);
      } else {
        setRows(page.rows);
        setEscapedRows(page.escapedRows);
        setPending({});
        setSelected([]);
        setText(page.text ?? '');
        setTtlInput(page.ttl > 0 ? String(page.ttl) : '');
      }
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (loadedOnce.current) return;
    loadedOnce.current = true;
    load();
  }, []);

  const confirm = async (message: string, detail: string, button: string) => {
    if (!((await forge.settings.get<boolean>('dbExplorer.confirmChanges')) ?? true)) return true;
    return (await forge.window.confirm(message, { detail, buttons: [button, 'Cancel'], level: 'warning' })) === 0;
  };

  const act = async (what: () => Promise<unknown>, reload = true) => {
    setBusy(true);
    setError(null);
    try {
      await what();
      if (reload) await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  if (!value) {
    return (
      <View style={{ padding: 12, gap: 8 }}>
        {busy && <Spinner />}
        {error && (
          <Text style={{ color: 'error' }} selectable>
            {error}
          </Text>
        )}
      </View>
    );
  }

  const type = value.type;
  const changes = Object.keys(pending).length;
  const stringDirty = type === 'string' && text !== (value.text ?? '');

  const saveString = () =>
    act(async () => {
      if (!(await confirm(`Save ${name}?`, 'It replaces the value (its TTL stays).', 'Save'))) return;
      await redis.edit(ref, { op: 'setString', value: text }, value.escaped);
    });

  const saveRows = () =>
    act(async () => {
      if (!(await confirm(`Save ${changes} change${changes === 1 ? '' : 's'} to ${name}?`, 'They are written one by one.', 'Save'))) return;
      // Deleting list items by index: highest first is not needed (they are marked first), but
      // edits by index must come before deletions shift them.
      const order = Object.entries(pending).sort(([, a], [, b]) => Number('deleted' in a) - Number('deleted' in b));
      const listDeletes: number[] = [];
      for (const [row, change] of order) {
        const r = rows[Number(row)];
        const escaped = escapedRows[Number(row)] ?? false;
        if (type === 'list' && 'deleted' in change) {
          listDeletes.push(Number(r[0]));
          continue;
        }
        for (const edit of editsFor(type, r, change)) await redis.edit(ref, edit, escaped);
      }
      if (listDeletes.length) await redis.edit(ref, { op: 'listDelete', indexes: listDeletes });
    });

  const add = () =>
    act(async () => {
      let edit: RedisEdit;
      switch (type) {
        case 'hash':
          if (!addA) throw new Error('A field needs a name.');
          edit = { op: 'hashSet', field: addA, value: addB };
          break;
        case 'list':
          edit = { op: 'listPush', value: addB, head: listEnd === 'head' };
          break;
        case 'set':
          edit = { op: 'setAdd', member: addA };
          break;
        case 'zset': {
          const score = Number(addB || '0');
          if (!Number.isFinite(score)) throw new Error(`“${addB}” is not a score (a number)`);
          edit = { op: 'zSetAdd', member: addA, score };
          break;
        }
        case 'stream': {
          let fields: [string, string][];
          try {
            const parsed = JSON.parse(addB || '{}');
            fields = Object.entries(parsed).map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]);
          } catch {
            throw new Error('Fields are a JSON object: { "type": "login", "user": "1" }');
          }
          edit = { op: 'streamAdd', fields };
          break;
        }
        default:
          return;
      }
      await redis.edit(ref, edit);
      setAddA('');
      setAddB('');
    });

  const setTtl = (seconds: number | null) =>
    act(async () => {
      if (seconds !== null && !(seconds > 0)) throw new Error('A TTL is a number of seconds, at least 1.');
      await redis.expire(ref, seconds);
    });

  const rename = () =>
    act(async () => {
      const to = (renaming ?? '').trim();
      if (!to || to === name) return setRenaming(null);
      await redis.rename(ref, to);
      setName(to);
      setRenaming(null);
      onRenamed?.(to);
      await store.reloadRedisKeys(connectionId, db);
    }, false);

  const remove = () =>
    act(async () => {
      if (!(await confirm(`Delete the key ${name}?`, `It is removed from db${db}, with its whole value. This can't be undone.`, 'Delete'))) return;
      await redis.remove(connectionId, db, [{ name, escaped: keyEscaped }]);
      await store.reloadRedisKeys(connectionId, db);
      onDeleted?.();
    }, false);

  const rowStates: Record<number, GridRowState> = {};
  const editedCells: string[] = [];
  const shownRows = rows.map((r, i) => {
    const change = pending[i];
    if (!change) return r;
    if ('deleted' in change) {
      rowStates[i] = 'deleted';
      return r;
    }
    rowStates[i] = 'modified';
    return r.map((c, j) => {
      if (change.values[j] === undefined) return c;
      editedCells.push(`${i}:${j}`);
      return change.values[j];
    });
  });

  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 4, borderSide: 'bottom', wrap: true }}>
        <Text style={{ weight: 'medium', size: 'sm' }}>{TYPE_LABELS[type] ?? type}</Text>
        {renaming === null ? (
          <Text style={{ mono: true, size: 'sm', truncate: true }} selectable>
            {name}
          </Text>
        ) : (
          <View style={{ width: 280 }}>
            <Input value={renaming} onChange={setRenaming} onSubmit={rename} autoFocus />
          </View>
        )}
        {renaming === null ? (
          <Button icon="pencil" variant="ghost" tooltip="Rename" onClick={() => setRenaming(name)} />
        ) : (
          <>
            <Button label="Rename" variant="filled" onClick={rename} />
            <Button label="Cancel" variant="ghost" onClick={() => setRenaming(null)} />
          </>
        )}
        <Text style={{ size: 'sm', color: 'muted' }}>
          {type === 'string' ? `${value.length.toLocaleString()} bytes` : `${value.length.toLocaleString()} ${type === 'hash' ? 'fields' : type === 'stream' ? 'entries' : 'items'}`}
        </Text>
        <View style={{ grow: true }} />
        {busy && <Spinner />}
        <Text style={{ size: 'sm', color: value.ttl >= 0 ? 'warning' : 'muted' }}>TTL: {ttlText(value.ttl)}</Text>
        <View style={{ width: 90 }}>
          <Input value={ttlInput} placeholder="seconds" onChange={setTtlInput} onSubmit={() => setTtl(Number(ttlInput))} />
        </View>
        <Button label="Set TTL" variant="ghost" disabled={!ttlInput.trim()} onClick={() => setTtl(Number(ttlInput))} />
        {value.ttl >= 0 && <Button label="Persist" variant="ghost" tooltip="Remove the TTL (the key stays for ever)" onClick={() => setTtl(null)} />}
        <Button icon="rotate_cw" variant="ghost" tooltip="Reload" onClick={() => load()} />
        <Button icon="trash" variant="ghost" tooltip="Delete Key" onClick={remove} />
      </View>

      {error && (
        <View style={{ paddingX: 8, paddingY: 4 }}>
          <Text style={{ color: 'error', size: 'sm' }} selectable>
            {error}
          </Text>
        </View>
      )}

      {type === 'string' ? (
        <View style={{ grow: true, padding: 8, gap: 6 }}>
          {value.escaped && <Text style={{ size: 'sm', color: 'muted' }}>Not UTF-8: bytes show as \xNN (and \ as \\) and are saved from that form.</Text>}
          <View style={{ direction: 'row', gap: 6 }}>
            <Button label="Save" icon="check" variant="filled" disabled={!stringDirty || busy} tooltip="⌘Enter" onClick={saveString} />
            {stringDirty && <Button label="Revert" icon="undo" variant="ghost" onClick={() => setText(value.text ?? '')} />}
          </View>
          <Input value={text} multiline language={looksLikeJson(text) ? 'JSON' : undefined} onChange={setText} onSubmit={() => stringDirty && saveString()} />
        </View>
      ) : (
        <View style={{ grow: true }}>
          <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 4, borderSide: 'bottom' }}>
            <Button label={changes ? `Save ${changes}` : 'Save'} icon="check" variant="filled" disabled={!changes || busy} onClick={saveRows} />
            {changes > 0 && <Button label="Discard" icon="undo" variant="ghost" onClick={() => setPending({})} />}
            <Button
              label="Delete Selected"
              icon="trash"
              variant="ghost"
              disabled={!selected.length}
              onClick={() => setPending((p) => ({ ...p, ...Object.fromEntries(selected.map((i) => [i, { deleted: true } as Pending])) }))}
            />
            <View style={{ grow: true }} />
            <Text style={{ size: 'sm', color: 'muted' }}>
              {rows.length.toLocaleString()} of {value.length.toLocaleString()}
            </Text>
            {next !== null && <Button label="Load more" variant="ghost" disabled={busy} onClick={() => load(true)} />}
          </View>
          <DataGrid
            style={{ grow: true }}
            columns={value.columns.map((c) => ({ name: c }))}
            rows={shownRows}
            selectedRows={selected}
            rowStates={rowStates}
            editedCells={editedCells}
            editable={editableColumns[type]?.length > 0}
            emptyText={busy ? 'Loading…' : 'Empty'}
            onSelect={setSelected}
            onCellEdit={({ row, column, value: v }) => {
              if (!editableColumns[type]?.includes(column)) return;
              setPending((p) => {
                const current = p[row];
                const values = current && !('deleted' in current) ? { ...current.values } : {};
                values[column] = v;
                return { ...p, [row]: { values } };
              });
            }}
            onDeleteRows={(r) => setPending((p) => ({ ...p, ...Object.fromEntries(r.map((i) => [i, { deleted: true } as Pending])) }))}
            contextMenu={[
              { id: 'copy', label: 'Copy Value', icon: 'copy' },
              { id: 'delete', label: 'Delete', icon: 'trash', danger: true },
            ]}
            onContextMenu={(e) => {
              if (e.id === 'copy') forge.clipboard.writeText(String(shownRows[e.row]?.[e.column] ?? ''));
              else setPending((p) => ({ ...p, ...Object.fromEntries(e.rows.map((i) => [i, { deleted: true } as Pending])) }));
            }}
          />
          <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 6, borderSide: 'top' }}>
            {type === 'hash' && (
              <>
                <View style={{ width: 200 }}>
                  <Input value={addA} placeholder="New field" onChange={setAddA} onSubmit={add} />
                </View>
                <View style={{ grow: true }}>
                  <Input value={addB} placeholder="Value" onChange={setAddB} onSubmit={add} />
                </View>
              </>
            )}
            {type === 'list' && (
              <>
                <View style={{ grow: true }}>
                  <Input value={addB} placeholder="New item" onChange={setAddB} onSubmit={add} />
                </View>
                <Select
                  value={listEnd}
                  options={[
                    { value: 'tail', label: 'At the end (RPUSH)' },
                    { value: 'head', label: 'At the start (LPUSH)' },
                  ]}
                  onChange={setListEnd}
                />
              </>
            )}
            {(type === 'set' || type === 'zset') && (
              <View style={{ grow: true }}>
                <Input value={addA} placeholder="New member" onChange={setAddA} onSubmit={add} />
              </View>
            )}
            {type === 'zset' && (
              <View style={{ width: 120 }}>
                <Input value={addB} placeholder="Score" onChange={setAddB} onSubmit={add} />
              </View>
            )}
            {type === 'stream' && (
              <View style={{ grow: true }}>
                <Input value={addB} language="JSON" placeholder='New entry: { "type": "login", "user": "1" }' onChange={setAddB} onSubmit={add} />
              </View>
            )}
            <Button label="Add" icon="plus" disabled={busy} onClick={add} />
          </View>
        </View>
      )}
    </View>
  );
}

/** A new key: its name, type and first value (a hash's or stream's first field), and a TTL. */
export function NewKeyView({ connectionId, db, onCreated }: { connectionId: string; db: number; onCreated: (key: string, type: string) => void }) {
  const [name, setName] = useState('');
  const [type, setType] = useState('string');
  const [field, setField] = useState('');
  const [value, setValue] = useState('');
  const [score, setScore] = useState('');
  const [ttl, setTtl] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const needsField = type === 'hash' || type === 'stream';

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      if (!name.trim()) throw new Error('The key needs a name.');
      if (needsField && !field.trim()) throw new Error(type === 'hash' ? 'A hash starts with a field.' : 'A stream entry needs a field.');
      if (ttl.trim() && !(Number(ttl) > 0)) throw new Error('A TTL is a number of seconds, at least 1.');
      if (!(await store.ensureConnected(connectionId))) throw new Error('Not connected.');
      const ref = { connectionId, db, key: name.trim() };
      await redis.edit(ref, { op: 'create', type, field: needsField ? field : undefined, value, score: type === 'zset' ? Number(score || '0') : undefined });
      if (ttl.trim()) await redis.expire(ref, Number(ttl));
      await store.reloadRedisKeys(connectionId, db);
      onCreated(name.trim(), type);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const label = (text: string) => <Text style={{ size: 'sm', color: 'muted' }}>{text}</Text>;
  return (
    <View style={{ padding: 20, gap: 12, maxWidth: 560 }}>
      <Text style={{ size: 'lg', weight: 'bold' }}>New Key in db{db}</Text>
      {label('Name')}
      <Input value={name} placeholder="user:42:profile" onChange={setName} autoFocus />
      {label('Type')}
      <Select value={type} options={Object.entries(TYPE_LABELS).map(([value, label]) => ({ value, label }))} onChange={setType} />
      {needsField && (
        <>
          {label(type === 'hash' ? 'First field' : 'Field of the first entry')}
          <Input value={field} onChange={setField} />
        </>
      )}
      {label(type === 'string' ? 'Value' : type === 'set' || type === 'zset' ? 'First member' : type === 'list' ? 'First item' : 'Value')}
      <Input value={value} multiline={type === 'string'} language={type === 'string' && looksLikeJson(value) ? 'JSON' : undefined} onChange={setValue} />
      {type === 'zset' && (
        <>
          {label('Score')}
          <Input value={score} placeholder="0" onChange={setScore} />
        </>
      )}
      {label('TTL in seconds (optional)')}
      <Input value={ttl} placeholder="No expiry" onChange={setTtl} onSubmit={create} />
      <View style={{ direction: 'row', gap: 8 }}>
        <Button label="Create" icon="check" variant="filled" disabled={busy} onClick={create} />
        {busy && <Spinner />}
      </View>
      {error && (
        <Text style={{ color: 'error', size: 'sm' }} selectable>
          {error}
        </Text>
      )}
    </View>
  );
}
