import { useEffect, useState } from 'react';
import { forge, Button, Checkbox, Divider, Icon, Input, Text, View } from '@forge/api';
import type { ExtensionContext } from '@forge/api';

type Todo = { id: number; text: string; done: boolean };

function Notes() {
  const [todos, setTodos] = useState<Todo[]>([
    { id: 1, text: 'Try the Forge extension API', done: true },
    { id: 2, text: 'Write my own panel in React', done: false },
  ]);
  const [draft, setDraft] = useState('');
  const [roots, setRoots] = useState<string[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [title, setTitle] = useState('Notes');
  const [showWorkspace, setShowWorkspace] = useState(true);

  // Settings declared in package.json, edited in Forge › Settings.
  useEffect(() => {
    forge.settings.get<string>('notes.title').then((t) => setTitle(t ?? 'Notes'));
    forge.settings.get<boolean>('notes.showWorkspace').then((s) => setShowWorkspace(s ?? true));
    const sub = forge.settings.onDidChange((key, value) => {
      if (key === 'notes.title') setTitle((value as string | null) ?? 'Notes');
      if (key === 'notes.showWorkspace') setShowWorkspace((value as boolean | null) ?? true);
    });
    return () => sub.dispose();
  }, []);

  const refresh = async () => {
    setRoots(await forge.workspace.roots());
    setActive(await forge.workspace.activeFile());
  };
  useEffect(() => { refresh().catch((e) => console.error(e)); }, []);

  const add = (text: string) => {
    if (!text.trim()) return;
    setTodos((ts) => [...ts, { id: Date.now(), text: text.trim(), done: false }]);
    setDraft('');
  };
  const remaining = todos.filter((t) => !t.done).length;

  return (
    <View style={{ gap: 12 }}>
      <View style={{ direction: 'row', justify: 'between' }}>
        <Text style={{ weight: 'bold', size: 'lg' }}>{title}</Text>
        <Text style={{ color: remaining ? 'accent' : 'success', size: 'sm' }}>
          {remaining ? `${remaining} pending` : 'All done'}
        </Text>
      </View>

      <Input value={draft} placeholder="Add a note and press Enter" onChange={setDraft} onSubmit={add} />

      <View style={{ gap: 4 }}>
        {todos.map((t) => (
          <View key={t.id} style={{ direction: 'row', justify: 'between' }}>
            <Checkbox
              checked={t.done}
              label={t.text}
              onChange={(done) => setTodos((ts) => ts.map((x) => (x.id === t.id ? { ...x, done } : x)))}
            />
            <Button icon="trash" variant="ghost" onClick={() => setTodos((ts) => ts.filter((x) => x.id !== t.id))} />
          </View>
        ))}
      </View>

      {showWorkspace && <Divider />}

      {showWorkspace && <View style={{ gap: 6 }}>
        <View style={{ direction: 'row', justify: 'between' }}>
          <Text style={{ weight: 'bold' }}>Workspace</Text>
          <Button icon="rotate_cw" label="Refresh" variant="ghost" onClick={() => refresh()} />
        </View>
        {roots.map((r) => (
          <View key={r} style={{ direction: 'row', gap: 6 }}>
            <Icon name="folder" style={{ color: 'muted' }} />
            <Text style={{ mono: true, size: 'sm' }}>{r}</Text>
          </View>
        ))}
        <Text style={{ color: 'muted', size: 'sm' }}>Active file: {active ?? 'none'}</Text>
        <Button
          icon="file"
          label="Open README"
          onClick={() => forge.workspace.openFile('README.md').catch((e) => forge.window.showMessage(String(e), 'error'))}
        />
      </View>}
    </View>
  );
}

export function activate(ctx: ExtensionContext) {
  ctx.subscriptions.push(
    forge.panels.register({ id: 'workspace-notes', title: 'Notes', icon: 'notepad', render: () => <Notes /> }),
    forge.commands.register('workspace-notes.hello', 'Say hello', () => forge.window.showMessage('Hello from a React extension 👋')),
  );
}
