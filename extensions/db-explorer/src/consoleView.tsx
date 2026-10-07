// The Redis console: command lines run on the chosen database, answered as `redis-cli`
// prints them (`1) "a"`, `(integer) 5`, `(error) …`). Commands that would take the
// connection over (SUBSCRIBE, MONITOR…) are refused; SELECT is the database menu.
import { useEffect, useRef, useState } from 'react';
import { Button, Input, Scroll, Select, Spinner, Text, View } from '@forge/api';
import { redis, requestId, sql } from './client';
import * as store from './store';

type Entry = { db: number; line: string; output: string; error: boolean; ms?: number };

/** At most this many entries stay on screen. */
const KEEP = 200;

/** Commands that may create, rename or delete keys: after them the tree looks again. */
const KEY_COMMANDS = new Set(
  'set setnx setex psetex mset msetnx getset getdel del unlink rename renamenx copy move restore flushdb flushall swapdb expire pexpire expireat pexpireat persist incr incrby incrbyfloat decr decrby append setrange hset hsetnx hmset hdel hincrby hincrbyfloat lpush rpush lpushx rpushx lpop rpop lmpop blpop brpop lmove blmove rpoplpush lset lrem ltrim linsert sadd srem spop smove sinterstore sunionstore sdiffstore zadd zrem zincrby zpopmin zpopmax zmpop zremrangebyscore zremrangebyrank zremrangebylex zunionstore zinterstore zdiffstore zrangestore xadd xdel xtrim pfadd pfmerge setbit bitop geoadd json.set json.del'.split(' '),
);
export const changesKeys = (command: string) => KEY_COMMANDS.has(command.trim().split(/\s+/)[0]?.toLowerCase() ?? '');

export function ConsoleView({ connectionId, db: initialDb }: { connectionId: string; db: number }) {
  const [db, setDb] = useState(initialDb);
  const [databases, setDatabases] = useState<number[]>([]);
  const [line, setLine] = useState('');
  const [entries, setEntries] = useState<Entry[]>([]);
  const [running, setRunning] = useState<string | null>(null);
  /** What was run, to go back to it (newest last). */
  const history = useRef<string[]>([]);
  const [back, setBack] = useState(0);

  useEffect(() => {
    store.ensureConnected(connectionId).then((ok) => {
      if (ok) redis.databases(connectionId).then((d) => setDatabases(d.map((x) => x.db))).catch(() => {});
    });
  }, [connectionId]);

  const run = async (text = line) => {
    const command = text.trim();
    if (!command || running) return;
    const id = requestId();
    setRunning(id);
    setLine('');
    setBack(0);
    history.current = [...history.current.filter((h) => h !== command), command].slice(-100);
    try {
      if (!(await store.ensureConnected(connectionId))) throw new Error('Not connected.');
      const result = await redis.command(connectionId, db, command, id);
      setEntries((e) => [...e, { db, line: command, output: result.output, error: result.output.startsWith('(error)'), ms: result.elapsedMs }].slice(-KEEP));
      if (changesKeys(command)) store.reloadRedisKeys(connectionId, db);
    } catch (e) {
      setEntries((x) => [...x, { db, line: command, output: (e as Error).message, error: true }].slice(-KEEP));
    } finally {
      setRunning(null);
    }
  };

  /** The previous (or next) command run, into the input. */
  const recall = (step: number) => {
    const h = history.current;
    const at = Math.min(h.length, Math.max(0, back + step));
    setBack(at);
    setLine(at === 0 ? '' : h[h.length - at]);
  };

  const config = store.connection(connectionId);
  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 6, paddingX: 8, paddingY: 4, borderSide: 'bottom' }}>
        <Text style={{ size: 'sm', weight: 'medium' }}>{config?.name ?? connectionId}</Text>
        <Select value={db} options={(databases.length ? databases : [db]).map((d) => ({ value: d, label: `db${d}` }))} onChange={setDb} />
        <View style={{ grow: true }} />
        {running && <Spinner />}
        {running && <Button label="Cancel" icon="stop" variant="ghost" onClick={() => sql.cancel(running)} />}
        <Button icon="arrow_up" variant="ghost" tooltip="Previous command" disabled={back >= history.current.length} onClick={() => recall(1)} />
        <Button icon="arrow_down" variant="ghost" tooltip="Next command" disabled={back === 0} onClick={() => recall(-1)} />
        <Button label="Clear" variant="ghost" disabled={!entries.length} onClick={() => setEntries([])} />
      </View>
      <Scroll style={{ grow: true, padding: 8, gap: 8 }}>
        {entries.length === 0 && <Text style={{ color: 'muted', size: 'sm' }}>Type a command and press Enter: GET user:1, HGETALL session:42, INFO memory, SCAN 0 MATCH user:* …</Text>}
        {entries.map((e, i) => (
          <View key={i} style={{ gap: 2 }}>
            <Text style={{ mono: true, size: 'sm', color: 'accent' }} selectable>
              {`db${e.db}> ${e.line}`}
            </Text>
            <Text style={{ mono: true, size: 'sm', color: e.error ? 'error' : undefined }} selectable>
              {e.output}
            </Text>
          </View>
        ))}
      </Scroll>
      <View style={{ padding: 8, borderSide: 'top' }}>
        <Input value={line} placeholder={`db${db}> command (Enter runs it)`} onChange={setLine} onSubmit={() => run()} autoFocus />
      </View>
    </View>
  );
}
