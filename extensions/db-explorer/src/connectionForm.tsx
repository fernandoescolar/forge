// Adding or editing a connection: engine, where the server (or SQLite file) is, who to
// sign in as. The password goes to the keychain when "Save password" is on.
import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';
import { forge, Button, Checkbox, Input, Scroll, Select, Spinner, Text, View } from '@forge/api';
import { sql } from './client';
import type { Engine } from './client';
import * as store from './store';
import type { ConnectionConfig } from './store';

type Form = {
  name: string;
  engine: Engine;
  host: string;
  port: string;
  user: string;
  password: string;
  database: string;
  file: string;
  ssl: 'disable' | 'prefer' | 'require';
  trustServerCertificate: boolean;
  savePassword: boolean;
};

const blank: Form = { name: '', engine: 'postgres', host: 'localhost', port: '', user: '', password: '', database: '', file: '', ssl: 'prefer', trustServerCertificate: true, savePassword: true };

export function ConnectionForm({ id, message, onDone }: { id: string | null; message?: string; onDone: () => void }) {
  const [form, setForm] = useState<Form>(blank);
  const [status, setStatus] = useState<{ kind: 'idle' | 'busy' | 'ok' | 'error'; text?: string }>(message ? { kind: 'error', text: message } : { kind: 'idle' });

  useEffect(() => {
    if (!id) return;
    const c = store.connection(id);
    if (!c) return;
    store.storedPassword(id).then((password) =>
      setForm({
        name: c.name,
        engine: c.engine,
        host: c.host ?? '',
        port: c.port ? String(c.port) : '',
        user: c.user ?? '',
        password: password ?? '',
        database: c.database ?? '',
        file: c.file ?? '',
        ssl: c.ssl ?? 'prefer',
        trustServerCertificate: c.trustServerCertificate ?? true,
        savePassword: c.savePassword,
      }),
    );
  }, [id]);

  const set = <K extends keyof Form>(key: K) => (value: Form[K]) => setForm((f) => ({ ...f, [key]: value }));
  const sqlite = form.engine === 'sqlite';
  const defaultPort = store.ENGINES.find((e) => e.value === form.engine)?.port;

  const config = (): ConnectionConfig => ({
    id: id ?? store.newId(),
    name: form.name.trim() || (sqlite ? form.file.split('/').pop() ?? 'SQLite' : `${form.database || form.host}`) || 'Connection',
    engine: form.engine,
    host: sqlite ? undefined : form.host.trim() || 'localhost',
    port: sqlite || !form.port.trim() ? undefined : Number(form.port),
    user: sqlite ? undefined : form.user.trim() || undefined,
    database: sqlite ? undefined : form.database.trim() || undefined,
    file: sqlite ? form.file.trim() : undefined,
    ssl: form.engine === 'postgres' || form.engine === 'mysql' || form.engine === 'mariadb' ? form.ssl : undefined,
    trustServerCertificate: form.engine === 'mssql' ? form.trustServerCertificate : undefined,
    savePassword: form.savePassword,
  });

  const invalid = sqlite ? !form.file.trim() : !form.host.trim() || (!!form.port.trim() && !/^\d+$/.test(form.port.trim()));

  const test = async () => {
    setStatus({ kind: 'busy', text: 'Connecting…' });
    const probe = `test-${Date.now()}`;
    try {
      const result = await sql.connect(probe, store.connectParams(config(), sqlite ? null : form.password));
      await sql.disconnect(probe).catch(() => {});
      setStatus({ kind: 'ok', text: `Connected: ${result.serverVersion}` });
    } catch (e) {
      setStatus({ kind: 'error', text: (e as Error).message });
    }
  };

  const save = async (connect: boolean) => {
    const c = config();
    await store.saveConnection(c, sqlite ? null : form.password);
    onDone();
    if (connect) {
      const node = store.visibleRows().find((r) => r.node.key === c.id)?.node;
      if (node && !store.isExpanded(node.key)) await store.toggle(node);
    }
  };

  const browse = async () => {
    const files = await forge.window.pickFiles({ prompt: 'Open' });
    if (files?.[0]) {
      setForm((f) => ({ ...f, file: files[0], name: f.name || files[0].split('/').pop() || '' }));
    }
  };
  const create = async () => {
    const file = await forge.window.saveFile({ name: 'database.sqlite' });
    if (!file) return;
    // An empty file is a valid, empty SQLite database.
    await forge.process.exec('touch', { args: [file] });
    setForm((f) => ({ ...f, file, name: f.name || file.split('/').pop() || '' }));
  };

  return (
    <Scroll style={{ grow: true }}>
      <View style={{ padding: 20, gap: 14, maxWidth: 560 }}>
        <Text style={{ size: 'lg', weight: 'bold' }}>{id ? 'Edit Connection' : 'New Connection'}</Text>

        <Field label="Database">
          <Select value={form.engine} options={store.ENGINES.map((e) => ({ value: e.value, label: e.label }))} onChange={(engine) => setForm((f) => ({ ...f, engine, port: '' }))} />
        </Field>
        <Field label="Name">
          <Input value={form.name} placeholder="Shown in the Databases panel" onChange={set('name')} autoFocus />
        </Field>

        {sqlite ? (
          <Field label="File">
            <View style={{ direction: 'row', gap: 6 }}>
              <View style={{ grow: true }}>
                <Input value={form.file} placeholder="/path/to/database.sqlite" onChange={set('file')} />
              </View>
              <Button label="Browse…" onClick={browse} />
              <Button label="New…" tooltip="Create an empty database file" onClick={create} />
            </View>
          </Field>
        ) : (
          <>
            <View style={{ direction: 'row', gap: 10 }}>
              <View style={{ grow: true }}>
                <Field label="Host">
                  <Input value={form.host} placeholder="localhost" onChange={set('host')} />
                </Field>
              </View>
              <View style={{ width: 110 }}>
                <Field label="Port">
                  <Input value={form.port} placeholder={defaultPort ? String(defaultPort) : ''} onChange={set('port')} />
                </Field>
              </View>
            </View>
            <View style={{ direction: 'row', gap: 10 }}>
              <View style={{ grow: true }}>
                <Field label="User">
                  <Input value={form.user} placeholder={form.engine === 'mssql' ? 'sa' : form.engine === 'postgres' ? 'postgres' : 'root'} onChange={set('user')} />
                </Field>
              </View>
              <View style={{ grow: true }}>
                <Field label="Password">
                  <Input value={form.password} password onChange={set('password')} onSubmit={() => !invalid && save(true)} />
                </Field>
              </View>
            </View>
            <Checkbox checked={form.savePassword} label="Save password in the keychain" onChange={set('savePassword')} />
            <Field label="Database (optional)">
              <Input value={form.database} placeholder={form.engine === 'postgres' ? 'postgres' : form.engine === 'mssql' ? 'master' : ''} onChange={set('database')} />
            </Field>
            {form.engine === 'mssql' ? (
              <Checkbox checked={form.trustServerCertificate} label="Trust the server certificate" onChange={set('trustServerCertificate')} />
            ) : (
              <Field label="SSL">
                <Select
                  value={form.ssl}
                  options={[
                    { value: 'disable', label: 'Disable' },
                    { value: 'prefer', label: 'Prefer' },
                    { value: 'require', label: 'Require' },
                  ]}
                  onChange={set('ssl')}
                />
              </Field>
            )}
          </>
        )}

        <View style={{ direction: 'row', gap: 8 }}>
          <Button label="Save and Connect" icon="check" variant="filled" disabled={invalid} onClick={() => save(true)} />
          <Button label="Save" disabled={invalid} onClick={() => save(false)} />
          <Button label="Test" icon="link" disabled={invalid || status.kind === 'busy'} onClick={test} />
          <Button label="Cancel" variant="ghost" onClick={onDone} />
        </View>
        {status.kind !== 'idle' && (
          <View style={{ direction: 'row', gap: 6 }}>
            {status.kind === 'busy' && <Spinner />}
            <Text style={{ color: status.kind === 'error' ? 'error' : status.kind === 'ok' ? 'success' : 'muted', size: 'sm' }}>{status.text}</Text>
          </View>
        )}
      </View>
    </Scroll>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <View style={{ gap: 4 }}>
      <Text style={{ size: 'sm', color: 'muted' }}>{label}</Text>
      {children}
    </View>
  );
}
