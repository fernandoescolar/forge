// The Docker CLI (`containers.command`: docker, or Podman's compatible CLI), run with the
// project's shell environment, so DOCKER_HOST and contexts (OrbStack, Colima…) apply.
import { forge } from '@forge-ide/api';
import type { ChildProcess } from '@forge-ide/api';

export type Container = {
  id: string;
  name: string;
  image: string;
  /** created, running, paused, restarting, exited, dead… */
  state: string;
  /** `Up 2 hours (healthy)`, `Exited (0) 3 days ago`… */
  status: string;
  ports: string;
  /** Compose labels, when Compose started it. */
  project: string | null;
  service: string | null;
  workingDir: string | null;
  configFiles: string | null;
};

export type Image = { id: string; repository: string; tag: string; size: string; created: string; containers: string };

let command = 'docker';

export function setCommand(cmd: string | null) {
  command = cmd?.trim() || 'docker';
}

export const commandName = () => command;

/** Runs the CLI; its output, or an error with what it printed. */
export async function docker(args: string[], cwd?: string): Promise<string> {
  let result;
  try {
    result = await forge.process.exec(command, { args, cwd });
  } catch (e) {
    throw new Error(`Couldn't run ${command}: ${e instanceof Error ? e.message : e}. Is Docker installed (or set Containers › Docker command)?`);
  }
  if (result.code !== 0) throw new Error((result.stderr || result.stdout).trim() || `${command} ${args[0]} failed (${result.code})`);
  return result.stdout;
}

const jsonLines = <T>(text: string): T[] =>
  text
    .split('\n')
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l) as T);

// Labels picked one by one: the `Labels` column is a comma-separated string, and Compose's
// values (several config files) have commas of their own.
const label = (name: string) => `{{json (.Label "${name}")}}`;
const PS_FORMAT = `{"id":{{json .ID}},"name":{{json .Names}},"image":{{json .Image}},"state":{{json .State}},"status":{{json .Status}},"ports":{{json .Ports}},"project":${label('com.docker.compose.project')},"service":${label('com.docker.compose.service')},"workingDir":${label('com.docker.compose.project.working_dir')},"configFiles":${label('com.docker.compose.project.config_files')}}`;

export async function containers(): Promise<Container[]> {
  const list = jsonLines<Container>(await docker(['ps', '-a', '--format', PS_FORMAT]));
  return list.map((c) => ({ ...c, id: c.id.slice(0, 12), project: c.project || null, service: c.service || null, workingDir: c.workingDir || null, configFiles: c.configFiles || null }));
}

export async function images(): Promise<Image[]> {
  type Raw = { ID: string; Repository: string; Tag: string; Size: string; CreatedSince: string; Containers: string };
  return jsonLines<Raw>(await docker(['images', '--format', '{{json .}}'])).map((i) => ({
    id: i.ID.replace(/^sha256:/, '').slice(0, 12),
    repository: i.Repository,
    tag: i.Tag,
    size: i.Size,
    created: i.CreatedSince,
    containers: i.Containers,
  }));
}

export type Action = 'start' | 'stop' | 'restart' | 'pause' | 'unpause' | 'rm';

export const act = (action: Action, ids: string[]) => docker(action === 'rm' ? ['rm', '-f', ...ids] : [action, ...ids]);

export const removeImage = (id: string) => docker(['rmi', id]);

export const inspect = (id: string) => docker(['inspect', id]);

/** The last `lines` of a container's logs (both streams, which Docker keeps apart). */
export async function logs(id: string, lines: number): Promise<string> {
  const result = await forge.process.exec(command, { args: ['logs', '--tail', String(lines), '--timestamps', id] });
  if (result.code !== 0) throw new Error(result.stderr.trim() || `${command} logs failed`);
  return [result.stdout, result.stderr].filter((s) => s.trim()).join('\n');
}

/**
 * The arguments that address a Compose project: its name, and its folder and files when the
 * labels say (so `up` finds the compose files from anywhere).
 */
export function composeArgs(project: { name: string; workingDir: string | null; configFiles: string | null }): string[] {
  const args = ['compose', '-p', project.name];
  if (project.workingDir) args.push('--project-directory', project.workingDir);
  for (const file of project.configFiles?.split(',') ?? []) if (file.trim()) args.push('-f', file.trim());
  return args;
}

export type ComposeAction = 'start' | 'stop' | 'restart' | 'down' | 'up';

export function compose(project: { name: string; workingDir: string | null; configFiles: string | null }, action: ComposeAction, services: string[] = []) {
  const verb = action === 'up' ? ['up', '-d'] : [action];
  return docker([...composeArgs(project), ...verb, ...services], project.workingDir ?? undefined);
}

/** `docker compose up -d` in `cwd` (its compose file), for projects not running yet. */
export const composeUpIn = (cwd: string, file: string | undefined, services: string[] = []) => docker(['compose', ...(file ? ['-f', file] : []), 'up', '-d', ...services], cwd);

/** Follows Docker's container events (start, die, destroy…), one JSON object per line. */
export function events(): Promise<ChildProcess> {
  return forge.process.spawn(command, { args: ['events', '--filter', 'type=container', '--filter', 'type=image', '--format', '{{json .}}'] });
}

export const followLogs = (c: Container, lines: number) => forge.terminal.run(command, { args: ['logs', '-f', '--tail', String(lines), c.id], title: `Logs: ${c.name}` });

export const openShell = (c: Container, shell: string) => forge.terminal.run(command, { args: ['exec', '-it', c.id, shell], title: `Shell: ${c.name}` });
