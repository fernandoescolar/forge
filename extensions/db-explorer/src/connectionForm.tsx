// Adding or editing a connection: engine, where the server (or SQLite file) is, who to
// sign in as. The password goes to the keychain when "Save password" is on. MongoDB can
// also take a whole connection string (Atlas gives one), kept in the keychain too.
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
  /** MongoDB: connect with `url` instead of host, port, user and password. */
  useUrl: boolean;
  url: string;
};

const blank: Form = { name: '', engine: 'postgres', host: 'localhost', port: '', user: '', password: '', database: '', file: '', ssl: 'prefer', trustServerCertificate: true, savePassword: true, useUrl: false, url: '' };

/** `cluster0.abcd.mongodb.net` out of `mongodb+srv://user:pw@cluster0.abcd.mongodb.net/db?…`. */
const urlHost = (url: string) => url.replace(/^mongodb(\+srv)?:\/\//, '').replace(/^[^@/]*@/, '').split(/[/?,]/)[0];

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
        useUrl: !!c.useUrl,
        // With a connection string, the keychain holds it instead of a password.
        url: c.useUrl ? password ?? '' : '',
      }),
    );
  }, [id]);

  const set = <K extends keyof Form>(key: K) => (value: Form[K]) => setForm((f) => ({ ...f, [key]: value }));
  const sqlite = form.engine === 'sqlite';
  const mongodb = store.isMongo(form.engine);
  const withUrl = mongodb && form.useUrl;
  /** What goes to the keychain: the password, or the connection string. */
  const secret = sqlite ? null : withUrl ? form.url.trim() : form.password;
  const defaultPort = store.ENGINES.find((e) => e.value === form.engine)?.port;

  const config = (): ConnectionConfig => ({
    id: id ?? store.newId(),
    name: form.name.trim() || (sqlite ? form.file.split('/').pop() ?? 'SQLite' : withUrl ? urlHost(form.url) : `${form.database || form.host}`) || 'Connection',
    engine: form.engine,
    host: sqlite || withUrl ? undefined : form.host.trim() || 'localhost',
    port: sqlite || withUrl || !form.port.trim() ? undefined : Number(form.port),
    user: sqlite || withUrl ? undefined : form.user.trim() || undefined,
    database: sqlite || withUrl ? undefined : form.database.trim() || undefined,
    file: sqlite ? form.file.trim() : undefined,
    ssl: form.engine === 'postgres' || form.engine === 'mysql' || form.engine === 'mariadb' || (mongodb && !withUrl) ? form.ssl : undefined,
    trustServerCertificate: form.engine === 'mssql' || (mongodb && !withUrl && form.ssl === 'require') ? form.trustServerCertificate : undefined,
    useUrl: withUrl || undefined,
    savePassword: form.savePassword,
  });

  const invalid = sqlite ? !form.file.trim() : withUrl ? !/^mongodb(\+srv)?:\/\//.test(form.url.trim()) : !form.host.trim() || (!!form.port.trim() && !/^\d+$/.test(form.port.trim()));

  const test = async () => {
    setStatus({ kind: 'busy', text: 'Connecting…' });
    const probe = `test-${Date.now()}`;
    try {
      const result = await sql.connect(probe, store.connectParams(config(), secret));
      await sql.disconnect(probe).catch(() => {});
      setStatus({ kind: 'ok', text: `Connected: ${result.serverVersion}` });
    } catch (e) {
      setStatus({ kind: 'error', text: (e as Error).message });
    }
  };

  const save = async (connect: boolean) => {
    const c = config();
    await store.saveConnection(c, secret);
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
          <Select value={form.engine} options={store.ENGINES.map((e) => ({ value: e.value, label: e.label }))} onChange={(engine) => setForm((f) => ({ ...f, engine, port: '', ssl: store.isMongo(engine) ? 'disable' : f.ssl === 'disable' ? 'prefer' : f.ssl }))} />
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
        ) : withUrl ? (
          <>
            <Field label="Connect with">
              <Select value="url" options={[{ value: 'host', label: 'Host and port' }, { value: 'url', label: 'Connection string' }]} onChange={(v) => set('useUrl')(v === 'url')} />
            </Field>
            <Field label="Connection string">
              <Input value={form.url} placeholder="mongodb+srv://user:password@cluster0.example.mongodb.net/" onChange={set('url')} onSubmit={() => !invalid && save(true)} />
            </Field>
            <Checkbox checked={form.savePassword} label="Save the connection string in the keychain (it may hold the password)" onChange={set('savePassword')} />
          </>
        ) : (
          <>
            {mongodb && (
              <Field label="Connect with">
                <Select value="host" options={[{ value: 'host', label: 'Host and port' }, { value: 'url', label: 'Connection string' }]} onChange={(v) => set('useUrl')(v === 'url')} />
              </Field>
            )}
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
                  <Input value={form.user} placeholder={mongodb ? 'none' : form.engine === 'mssql' ? 'sa' : form.engine === 'postgres' ? 'postgres' : 'root'} onChange={set('user')} />
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
              <Input value={form.database} placeholder={form.engine === 'postgres' ? 'postgres' : form.engine === 'mssql' ? 'master' : mongodb ? 'Opened first; users still sign in against admin' : ''} onChange={set('database')} />
            </Field>
            {mongodb ? (
              <>
                <Field label="TLS">
                  <Select
                    value={form.ssl === 'require' ? 'require' : 'disable'}
                    options={[
                      { value: 'disable', label: 'Off' },
                      { value: 'require', label: 'On' },
                    ]}
                    onChange={set('ssl')}
                  />
                </Field>
                {form.ssl === 'require' && <Checkbox checked={form.trustServerCertificate} label="Accept any server certificate" onChange={set('trustServerCertificate')} />}
              </>
            ) : form.engine === 'mssql' ? (
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
