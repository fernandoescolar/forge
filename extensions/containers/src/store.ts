// The containers and images, kept current by following `docker events` (a change refreshes
// the lists), and what the panel shows: which view, the selection, the collapsed projects,
// and the containers an action is running on.
import { useSyncExternalStore } from 'react';
import { forge } from '@forge-ide/api';
import type { ChildProcess } from '@forge-ide/api';
import * as docker from './docker';
import type { Container, Image } from './docker';

export type Project = { name: string; workingDir: string | null; configFiles: string | null; containers: Container[] };

type State = {
  containers: Container[] | null;
  images: Image[] | null;
  /** Why the lists couldn't be read (Docker isn't running…). */
  error: string | null;
  view: 'containers' | 'images';
  /** A container's id, `project:<name>` or an image's id. */
  selected: string | null;
  collapsed: Set<string>;
  /** Containers (or `project:<name>`, image ids) an action is running on. */
  busy: Set<string>;
};

let state: State = { containers: null, images: null, error: null, view: 'containers', selected: null, collapsed: new Set(), busy: new Set() };
let version = 0;
const listeners = new Set<() => void>();

function set(changes: Partial<State>) {
  state = { ...state, ...changes };
  version++;
  listeners.forEach((l) => l());
}

export function useStore() {
  useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => version,
  );
  return state;
}

export const current = () => state;

export const setView = (view: State['view']) => set({ view, selected: null });
export const select = (selected: string | null) => set({ selected });

export function toggle(project: string) {
  const collapsed = new Set(state.collapsed);
  if (!collapsed.delete(project)) collapsed.add(project);
  set({ collapsed });
}

/** The containers by Compose project (by name), then the rest under `null`. */
export function projects(containers: Container[] = state.containers ?? []): { projects: Project[]; others: Container[] } {
  const byName = new Map<string, Project>();
  const others: Container[] = [];
  for (const c of containers) {
    if (!c.project) {
      others.push(c);
      continue;
    }
    let project = byName.get(c.project);
    if (!project) byName.set(c.project, (project = { name: c.project, workingDir: c.workingDir, configFiles: c.configFiles, containers: [] }));
    project.containers.push(c);
  }
  const byService = (a: Container, b: Container) => (a.service ?? a.name).localeCompare(b.service ?? b.name);
  const list = [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
  list.forEach((p) => p.containers.sort(byService));
  return { projects: list, others: others.sort((a, b) => a.name.localeCompare(b.name)) };
}

export function project(name: string): Project | undefined {
  return projects().projects.find((p) => p.name === name);
}

export function container(idOrName: string): Container | undefined {
  const wanted = idOrName.trim().replace(/^\//, '');
  return state.containers?.find((c) => c.id === wanted || c.name === wanted || (wanted.length >= 4 && c.id.startsWith(wanted)));
}

let refreshing: Promise<void> | null = null;
let again = false;

/** Reads the lists again (one read at a time; a request during one reads once more after it). */
export function refresh(): Promise<void> {
  if (refreshing) {
    again = true;
    return refreshing;
  }
  refreshing = (async () => {
    do {
      again = false;
      try {
        const [containers, images] = await Promise.all([docker.containers(), docker.images()]);
        set({ containers, images, error: null });
        if (!watching) watch();
      } catch (e) {
        set({ error: e instanceof Error ? e.message : String(e) });
      }
    } while (again);
    refreshing = null;
  })();
  return refreshing;
}

let events: ChildProcess | null = null;
let watching = false;
let timer: number | undefined;
let stopped = false;

/** Follows Docker's events: each burst of them refreshes the lists once. */
async function watch() {
  if (stopped) return;
  watching = true;
  try {
    events = await docker.events();
  } catch {
    watching = false;
    return;
  }
  events.onLine(() => {
    clearTimeout(timer);
    timer = setTimeout(() => refresh(), 250);
  });
  events.onExit(() => {
    events = null;
    watching = false;
    // Docker stopped (or restarted): see where things are, which watches again once it answers.
    if (!stopped) setTimeout(() => refresh(), 3000);
  });
}

export function init() {
  stopped = false;
  refresh();
}

export function stop() {
  stopped = true;
  events?.kill();
  clearTimeout(timer);
}

/** Runs `action` with `key` marked busy, shows its error, and refreshes (events may be off). */
export async function run(key: string, action: () => Promise<unknown>) {
  set({ busy: new Set(state.busy).add(key) });
  try {
    await action();
  } catch (e) {
    forge.window.showMessage(e instanceof Error ? e.message : String(e), 'error');
  } finally {
    const busy = new Set(state.busy);
    busy.delete(key);
    set({ busy });
    refresh();
  }
}
