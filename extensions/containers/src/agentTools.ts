// The Containers tools for agents: what is running and its logs (at once), and starting,
// stopping or restarting containers and Compose projects, or bringing one up (each asks the
// user first). Agents debugging a service see the same containers the user does.
import { forge } from '@forge-ide/api';
import type { Disposable } from '@forge-ide/api';
import * as docker from './docker';
import type { Container } from './docker';
import * as store from './store';
import { shortPorts } from './panel';

const MAX_LOG_LINES = 2000;
const MAX_LOG_CHARS = 60_000;

const describe = (c: Container) => `${c.name} (${c.image}): ${c.status}${c.ports ? `, ports ${shortPorts(c.ports)}` : ''}`;

async function fresh() {
  await store.refresh();
  const state = store.current();
  if (!state.containers) throw new Error(state.error ?? "Couldn't list the containers.");
  return state.containers;
}

/** A container by name, id or Compose service (when only one has it). */
async function find(name: string): Promise<Container> {
  const containers = await fresh();
  const found = store.container(name) ?? (() => {
    const matches = containers.filter((c) => c.service === name);
    return matches.length === 1 ? matches[0] : undefined;
  })();
  if (!found) throw new Error(`No container is called "${name}". The containers are: ${containers.map((c) => c.name).join(', ') || 'none'}.`);
  return found;
}

const targetSchema = {
  container: { type: 'string', description: 'A container\'s name or id (or a Compose service\'s name), as `containers` lists them.' },
  project: { type: 'string', description: 'A Compose project\'s name, for all its containers.' },
};

export function registerAgentTools(): Disposable[] {
  return [
    forge.agents.registerTool({
      name: 'containers',
      title: 'Containers',
      description: 'The Docker containers on this machine, by Compose project: name, image, status (running, exited, health) and published ports (host→container).',
      readOnly: true,
      run: async () => {
        const containers = await fresh();
        if (containers.length === 0) return 'There are no containers.';
        const { projects, others } = store.projects(containers);
        const parts = projects.map((p) => `Compose project ${p.name}${p.workingDir ? ` (${p.workingDir})` : ''}:\n${p.containers.map((c) => `- ${c.service ?? c.name}: ${describe(c)}`).join('\n')}`);
        if (others.length) parts.push(`${projects.length ? 'Other containers' : 'Containers'}:\n${others.map((c) => `- ${describe(c)}`).join('\n')}`);
        return parts.join('\n\n');
      },
    }),
    forge.agents.registerTool({
      name: 'logs',
      title: 'Container logs',
      description: `A container's last log lines (output and errors, with timestamps), at most ${MAX_LOG_LINES}; \`grep\` keeps the lines that contain it (ignoring case).`,
      readOnly: true,
      inputSchema: {
        type: 'object',
        properties: {
          container: targetSchema.container,
          lines: { type: 'integer', description: 'How many of the last lines (default 200).' },
          grep: { type: 'string' },
        },
        required: ['container'],
      },
      run: async (args: { container: string; lines?: number; grep?: string }) => {
        const c = await find(args.container);
        const lines = Math.min(Math.max(args.lines ?? 200, 1), MAX_LOG_LINES);
        let text = await docker.logs(c.id, args.grep ? MAX_LOG_LINES : lines);
        if (args.grep) {
          const wanted = args.grep.toLowerCase();
          text = text.split('\n').filter((l) => l.toLowerCase().includes(wanted)).slice(-lines).join('\n');
        }
        if (text.length > MAX_LOG_CHARS) text = `…\n${text.slice(-MAX_LOG_CHARS)}`;
        return text.trim() ? `${describe(c)}\n\n${text}` : `${describe(c)}\n\nNo log lines${args.grep ? ` contain "${args.grep}"` : ''}.`;
      },
    }),
    forge.agents.registerTool({
      name: 'control',
      title: 'Start, stop or restart containers',
      description: 'Starts, stops or restarts a container, or every container of a Compose project. The user approves each call.',
      inputSchema: {
        type: 'object',
        properties: { action: { enum: ['start', 'stop', 'restart'] }, ...targetSchema },
        required: ['action'],
      },
      run: async (args: { action: 'start' | 'stop' | 'restart'; container?: string; project?: string }) => {
        if (args.project) {
          await fresh();
          const project = store.project(args.project);
          if (!project) throw new Error(`No Compose project is called "${args.project}".`);
          await docker.compose(project, args.action);
          await store.refresh();
          return (store.project(args.project)?.containers ?? []).map((c) => `- ${describe(c)}`).join('\n') || 'Done.';
        }
        if (!args.container) throw new Error('Say which `container` or `project`.');
        const c = await find(args.container);
        await docker.act(args.action, [c.id]);
        await store.refresh();
        return describe(store.container(c.id) ?? c);
      },
    }),
    forge.agents.registerTool({
      name: 'compose_up',
      title: 'Compose up',
      description: 'Runs `docker compose up -d` (building what needs building) in a folder of the project, for its compose file or the one given: all its services, or those listed. The user approves each call.',
      inputSchema: {
        type: 'object',
        properties: {
          folder: { type: 'string', description: 'The folder with the compose file, relative to the project (default: its root).' },
          file: { type: 'string', description: 'The compose file, if not the folder\'s compose.yaml or docker-compose.yml.' },
          services: { type: 'array', items: { type: 'string' } },
        },
      },
      run: async (args: { folder?: string; file?: string; services?: string[] }, call) => {
        const folder = !args.folder ? call.cwd : args.folder.startsWith('/') ? args.folder : `${call.cwd}/${args.folder}`;
        await docker.composeUpIn(folder, args.file, args.services ?? []);
        const containers = await fresh();
        const started = containers.filter((c) => c.workingDir === folder);
        return started.length ? `Up:\n${started.map((c) => `- ${c.service ?? c.name}: ${describe(c)}`).join('\n')}` : 'Done.';
      },
    }),
  ];
}
