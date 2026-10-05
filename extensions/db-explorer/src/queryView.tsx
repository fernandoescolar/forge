// A query editor: write SQL, run it (⌘Enter), see each result set and what the statements
// did. Long queries can be cancelled.
import { useEffect, useRef, useState } from 'react';
import { forge, Button, DataGrid, Input, Select, Spinner, Tabs, Text, View } from '@forge/api';
import { requestId, sql } from './client';
import type { ResultSet } from './client';
import * as store from './store';

type Props = { connectionId: string; database: string | null; initialText: string };
type Outcome = { results: ResultSet[]; elapsedMs: number } | { error: string };

export function QueryView({ connectionId: initialConnection, database: initialDatabase, initialText }: Props) {
  const state = store.useStore();
  const [connectionId, setConnectionId] = useState(initialConnection);
  const [database, setDatabase] = useState<string | null>(initialDatabase);
  const [databases, setDatabases] = useState<string[]>([]);
  const [text, setText] = useState(initialText);
  const [running, setRunning] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [active, setActive] = useState<string>('messages');
  const [selected, setSelected] = useState<number[]>([]);
  const started = useRef(0);

  const config = store.connection(connectionId);

  useEffect(() => {
    setDatabases([]);
    if (!config || config.engine === 'sqlite') return;
    store.ensureConnected(connectionId).then((ok) => {
      if (ok) sql.listDatabases(connectionId).then(setDatabases).catch(() => {});
    });
  }, [connectionId]);

  const run = async (sqlText = text) => {
    if (!sqlText.trim() || running) return;
    const id = requestId();
    setRunning(id);
    started.current = Date.now();
    try {
      if (!(await store.ensureConnected(connectionId))) throw new Error('Not connected.');
      const maxRows = (await forge.settings.get<number>('dbExplorer.maxRows')) ?? 1000;
      const result = await sql.query(connectionId, sqlText, { database, maxRows, requestId: id });
      setOutcome({ results: result.resultSets, elapsedMs: result.elapsedMs });
      const firstRows = result.resultSets.findIndex((r) => r.columns.length > 0);
      setActive(firstRows >= 0 ? `r${firstRows}` : 'messages');
      setSelected([]);
    } catch (e) {
      setOutcome({ error: (e as Error).message });
      setActive('messages');
    } finally {
      setRunning(null);
    }
  };

  const results = outcome && 'results' in outcome ? outcome.results : [];
  const tabs = [
    ...results.flatMap((r, i) => (r.columns.length > 0 ? [{ id: `r${i}`, label: `Result ${results.filter((x, j) => j <= i && x.columns.length > 0).length} (${r.rows.length}${r.truncated ? '+' : ''})`, icon: 'table' }] : [])),
    { id: 'messages', label: 'Messages', icon: outcome && 'error' in outcome ? 'x_circle' : 'info' },
  ];
  const current = active.startsWith('r') ? results[Number(active.slice(1))] : null;

  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 4, borderSide: 'bottom' }}>
        {running ? (
          <Button label="Cancel" icon="stop" variant="filled" onClick={() => sql.cancel(running)} />
        ) : (
          <Button label="Run" icon="play_filled" variant="filled" tooltip="Run (⌘Enter)" disabled={!text.trim()} onClick={() => run()} />
        )}
        <Select value={connectionId} options={state.connections.map((c) => ({ value: c.id, label: c.name }))} onChange={(id) => { setConnectionId(id); setDatabase(null); }} />
        {databases.length > 0 && (
          <Select value={database} placeholder="Default database" options={databases.map((d) => ({ value: d, label: d }))} onChange={setDatabase} />
        )}
        {running && <Spinner />}
        <View style={{ grow: true }} />
        <Text style={{ size: 'sm', color: 'muted' }}>{config ? store.engineLabel(config.engine) : ''}</Text>
      </View>

      <View style={{ padding: 8, shrink: false }}>
        <Input value={text} multiline placeholder="SELECT … (⌘Enter runs it)" onChange={setText} onSubmit={run} autoFocus />
      </View>

      <Tabs tabs={tabs} active={active} onSelect={setActive} />
      {current ? (
        <DataGrid
          style={{ grow: true }}
          columns={current.columns.map((c) => ({ name: c.name, type: c.type }))}
          rows={current.rows}
          selectedRows={selected}
          onSelect={setSelected}
          emptyText="No rows"
          contextMenu={[
            { id: 'copy-value', label: 'Copy Value', icon: 'copy' },
            { id: 'copy-rows', label: 'Copy Rows', icon: 'copy' },
          ]}
          onContextMenu={(e) => {
            const cell = (v: unknown) => (v === null ? 'NULL' : String(v));
            if (e.id === 'copy-value') forge.clipboard.writeText(cell(current.rows[e.row]?.[e.column] ?? null));
            else forge.clipboard.writeText(e.rows.map((r) => current.rows[r].map(cell).join('\t')).join('\n'));
          }}
        />
      ) : (
        <View style={{ grow: true, padding: 10, gap: 6 }}>
          {!outcome && <Text style={{ color: 'muted' }}>Run a query to see its results.</Text>}
          {outcome && 'error' in outcome && <Text style={{ color: 'error', mono: true }}>{outcome.error}</Text>}
          {outcome && 'results' in outcome && (
            <>
              {outcome.results.map((r, i) => (
                <Text key={i} style={{ mono: true, size: 'sm' }}>
                  {r.columns.length > 0 ? `Statement ${i + 1}: ${r.rows.length}${r.truncated ? '+ (limited)' : ''} rows` : `Statement ${i + 1}: ${r.rowsAffected ?? 0} rows affected`}
                </Text>
              ))}
              <Text style={{ color: 'muted', size: 'sm' }}>Finished in {outcome.elapsedMs} ms.</Text>
            </>
          )}
        </View>
      )}
    </View>
  );
}
