// What the panel's buttons and menus do to a container, a Compose project or an image.
import { forge, Scroll, Text } from '@forge-ide/api';
import * as docker from './docker';
import type { Container, Image } from './docker';
import * as store from './store';
import type { Project } from './store';
import { confirmed, settings } from './settings';

export const isRunning = (c: Container) => c.state === 'running' || c.state === 'restarting';

export function containerAction(c: Container, action: docker.Action) {
  return store.run(c.id, () => docker.act(action, [c.id]));
}

export async function removeContainer(c: Container) {
  if (await confirmed(`Remove ${c.name}?`, isRunning(c) ? 'It is running: it will be stopped first. Its anonymous data goes with it.' : 'Its anonymous data goes with it.', 'Remove')) {
    await containerAction(c, 'rm');
  }
}

export function projectAction(p: Project, action: docker.ComposeAction) {
  return store.run(`project:${p.name}`, () => docker.compose(p, action));
}

export async function projectDown(p: Project) {
  if (await confirmed(`Take ${p.name} down?`, 'Its containers and networks are removed (its volumes are kept).', 'Down')) {
    await projectAction(p, 'down');
  }
}

export async function removeImage(image: Image) {
  const name = image.repository === '<none>' ? image.id : `${image.repository}:${image.tag}`;
  if (await confirmed(`Remove ${name}?`, 'Docker refuses while a container uses it.', 'Remove')) {
    await store.run(image.id, () => docker.removeImage(image.repository === '<none>' ? image.id : name));
  }
}

export const followLogs = (c: Container) => docker.followLogs(c, settings.logLines);

export function openShell(c: Container) {
  if (!isRunning(c)) return forge.window.showMessage(`${c.name} isn't running: start it first.`, 'warning');
  return docker.openShell(c, settings.shell);
}

export async function openComposeFile(p: Project) {
  const file = p.configFiles?.split(',')[0]?.trim();
  if (file) await forge.workspace.openFile(file);
  else forge.window.showMessage(`Docker didn't record ${p.name}'s compose file.`, 'warning');
}

/** `docker inspect` in a tab. */
export async function inspect(id: string, name: string) {
  let text: string;
  try {
    text = JSON.stringify(JSON.parse(await docker.inspect(id)), null, 2);
  } catch (e) {
    return forge.window.showMessage(e instanceof Error ? e.message : String(e), 'error');
  }
  forge.tabs.open({
    id: `containers.inspect.${id}`,
    title: `Inspect: ${name}`,
    icon: 'json',
    render: () => (
      <Scroll style={{ grow: true, padding: 8 }}>
        <Text selectable style={{ mono: true, size: 'sm' }}>{text}</Text>
      </Scroll>
    ),
  });
}

/** Compose files in the project's folders, for Compose Up. */
const COMPOSE_FILES = ['compose.yaml', 'compose.yml', 'docker-compose.yaml', 'docker-compose.yml'];

/** `docker compose up -d` for the active compose file, else the first project folder's. */
export async function composeUp() {
  const active = await forge.workspace.activeFile();
  const name = active?.split('/').pop() ?? '';
  if (active && (COMPOSE_FILES.includes(name) || /^(docker-)?compose\..+\.ya?ml$/.test(name))) {
    const folder = active.slice(0, active.length - name.length - 1);
    return store.run(`up:${active}`, () => docker.composeUpIn(folder, active));
  }
  const [root] = await forge.workspace.roots();
  if (!root) return forge.window.showMessage('Open a compose file or a project with one first.', 'warning');
  return store.run(`up:${root}`, () => docker.composeUpIn(root, undefined));
}
