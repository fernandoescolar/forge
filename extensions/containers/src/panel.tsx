// The Containers panel: containers by Compose project (then the others), or the images.
// Double-click a container to follow its logs; right-click anything for more.
import { forge, Button, Scroll, Text, TreeItem, View } from '@forge-ide/api';
import type { Color, MenuItem } from '@forge-ide/api';
import type { Container, Image } from './docker';
import * as store from './store';
import type { Project } from './store';
import * as actions from './actions';

const OTHERS = 'project:';

function stateColor(c: Container): Color {
  if (/\(unhealthy\)/.test(c.status) || c.state === 'dead') return 'error';
  if (c.state === 'running') return /\(health: starting\)/.test(c.status) ? 'warning' : 'success';
  if (c.state === 'paused' || c.state === 'restarting') return 'warning';
  return 'muted';
}

export function Panel() {
  const state = store.useStore();
  return (
    <View style={{ grow: true }}>
      <View style={{ direction: 'row', gap: 2, paddingX: 6, paddingY: 4, borderSide: 'bottom' }}>
        <Button label="Containers" variant="ghost" selected={state.view === 'containers'} onClick={() => store.setView('containers')} />
        <Button label="Images" variant="ghost" selected={state.view === 'images'} onClick={() => store.setView('images')} />
        <View style={{ grow: true }} />
        <Toolbar />
        <Button icon="rotate_cw" variant="ghost" tooltip="Refresh" onClick={() => store.refresh()} />
      </View>
      <Body />
    </View>
  );
}

/** The buttons for the selected container or project. */
function Toolbar() {
  const state = store.useStore();
  if (state.view !== 'containers' || !state.selected) return null;
  if (state.selected.startsWith('project:')) {
    const project = store.project(state.selected.slice('project:'.length));
    if (!project) return null;
    const busy = state.busy.has(state.selected);
    const anyRunning = project.containers.some(actions.isRunning);
    return (
      <>
        {anyRunning ? (
          <Button icon="stop" variant="ghost" tooltip="Stop" disabled={busy} onClick={() => actions.projectAction(project, 'stop')} />
        ) : (
          <Button icon="play_filled" variant="ghost" tooltip="Start" disabled={busy} onClick={() => actions.projectAction(project, 'start')} />
        )}
        <Button icon="rerun" variant="ghost" tooltip="Restart" disabled={busy} onClick={() => actions.projectAction(project, 'restart')} />
      </>
    );
  }
  const c = store.container(state.selected);
  if (!c) return null;
  const busy = state.busy.has(c.id);
  return (
    <>
      {actions.isRunning(c) ? (
        <Button icon="stop" variant="ghost" tooltip="Stop" disabled={busy} onClick={() => actions.containerAction(c, 'stop')} />
      ) : (
        <Button icon="play_filled" variant="ghost" tooltip="Start" disabled={busy} onClick={() => actions.containerAction(c, 'start')} />
      )}
      <Button icon="rerun" variant="ghost" tooltip="Restart" disabled={busy} onClick={() => actions.containerAction(c, 'restart')} />
      <Button icon="reader" variant="ghost" tooltip="Follow Logs" onClick={() => actions.followLogs(c)} />
      <Button icon="terminal" variant="ghost" tooltip="Open Shell" disabled={!actions.isRunning(c)} onClick={() => actions.openShell(c)} />
    </>
  );
}

function Body() {
  const state = store.useStore();
  const list = state.view === 'containers' ? state.containers : state.images;
  if (state.error && !list) {
    return (
      <View style={{ padding: 12, gap: 8 }}>
        <Text style={{ color: 'error' }}>{state.error}</Text>
        <Text style={{ color: 'muted', size: 'sm' }}>Is Docker running? Docker Desktop, OrbStack, Colima and Podman's Docker-compatible CLI all work.</Text>
        <Button label="Try Again" icon="rotate_cw" variant="filled" onClick={() => store.refresh()} />
      </View>
    );
  }
  if (!list) return <Text style={{ color: 'muted', padding: 12 }}>Loading…</Text>;
  return (
    <Scroll style={{ grow: true, padding: 4 }}>
      {state.error && <Text style={{ color: 'error', size: 'sm', padding: 4 }}>{state.error}</Text>}
      {state.view === 'containers' ? <Containers /> : <Images />}
    </Scroll>
  );
}

function Containers() {
  const state = store.useStore();
  const { projects, others } = store.projects();
  if (projects.length === 0 && others.length === 0) {
    return (
      <View style={{ padding: 8, gap: 8 }}>
        <Text style={{ color: 'muted' }}>No containers.</Text>
        <Button label="Compose Up" icon="play_filled" onClick={() => actions.composeUp()} />
      </View>
    );
  }
  return (
    <>
      {projects.map((p) => (
        <ProjectRows key={p.name} project={p} />
      ))}
      {others.length > 0 && projects.length > 0 && (
        <TreeItem label="Other containers" icon="box" expanded={!state.collapsed.has(OTHERS)} onToggle={() => store.toggle(OTHERS)} onClick={() => store.toggle(OTHERS)} />
      )}
      {(projects.length === 0 || !state.collapsed.has(OTHERS)) && others.map((c) => <ContainerRow key={c.id} container={c} depth={projects.length > 0 ? 1 : 0} />)}
    </>
  );
}

function ProjectRows({ project }: { project: Project }) {
  const state = store.useStore();
  const key = `project:${project.name}`;
  const running = project.containers.filter(actions.isRunning).length;
  const expanded = !state.collapsed.has(key);
  const menu: MenuItem[] = [
    { id: 'up', label: 'Up', icon: 'play_filled' },
    { id: 'start', label: 'Start', disabled: running === project.containers.length },
    { id: 'stop', label: 'Stop', icon: 'stop', disabled: running === 0 },
    { id: 'restart', label: 'Restart', icon: 'rerun' },
    { separator: true },
    { id: 'file', label: 'Open Compose File', icon: 'file_code', disabled: !project.configFiles },
    { separator: true },
    { id: 'down', label: 'Down…', icon: 'trash', danger: true },
  ];
  const onMenu = ({ id }: { id: string }) => {
    if (id === 'file') actions.openComposeFile(project);
    else if (id === 'down') actions.projectDown(project);
    else actions.projectAction(project, id as 'up' | 'start' | 'stop' | 'restart');
  };
  return (
    <>
      <TreeItem
        label={project.name}
        description={`${running}/${project.containers.length} running`}
        icon="blocks"
        iconColor={running === 0 ? 'muted' : running === project.containers.length ? 'success' : 'warning'}
        loading={state.busy.has(key)}
        expanded={expanded}
        selected={state.selected === key}
        onToggle={() => store.toggle(key)}
        onClick={() => store.select(key)}
        onDoubleClick={() => store.toggle(key)}
        contextMenu={menu}
        onContextMenu={onMenu}
      />
      {expanded && project.containers.map((c) => <ContainerRow key={c.id} container={c} depth={1} />)}
    </>
  );
}

function ContainerRow({ container: c, depth }: { container: Container; depth: number }) {
  const state = store.useStore();
  const running = actions.isRunning(c);
  const menu: MenuItem[] = [
    { id: 'logs', label: 'Follow Logs', icon: 'reader' },
    { id: 'shell', label: 'Open Shell', icon: 'terminal', disabled: !running },
    { separator: true },
    running ? { id: 'stop', label: 'Stop', icon: 'stop' } : { id: 'start', label: 'Start', icon: 'play_filled' },
    { id: 'restart', label: 'Restart', icon: 'rerun' },
    c.state === 'paused' ? { id: 'unpause', label: 'Resume' } : { id: 'pause', label: 'Pause', icon: 'debug_pause', disabled: c.state !== 'running' },
    { separator: true },
    { id: 'inspect', label: 'Inspect', icon: 'json' },
    { id: 'copy', label: 'Copy ID', icon: 'copy' },
    { separator: true },
    { id: 'rm', label: 'Remove…', icon: 'trash', danger: true },
  ];
  const onMenu = ({ id }: { id: string }) => {
    if (id === 'logs') actions.followLogs(c);
    else if (id === 'shell') actions.openShell(c);
    else if (id === 'inspect') actions.inspect(c.id, c.name);
    else if (id === 'copy') forge.clipboard.writeText(c.id);
    else if (id === 'rm') actions.removeContainer(c);
    else actions.containerAction(c, id as 'start' | 'stop' | 'restart' | 'pause' | 'unpause');
  };
  const ports = c.ports ? ` · ${shortPorts(c.ports)}` : '';
  return (
    <TreeItem
      label={depth > 0 && c.service ? c.service : c.name}
      description={`${c.status}${ports}`}
      icon="box"
      iconColor={stateColor(c)}
      depth={depth}
      loading={state.busy.has(c.id)}
      selected={state.selected === c.id}
      onClick={() => store.select(c.id)}
      onDoubleClick={() => actions.followLogs(c)}
      contextMenu={menu}
      onContextMenu={onMenu}
    />
  );
}

/** `0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp` → `5432→5432`. */
export function shortPorts(ports: string): string {
  const seen = new Set<string>();
  for (const part of ports.split(',')) {
    const m = part.trim().match(/:(\d+(?:-\d+)?)->(\d+(?:-\d+)?)\/(\w+)/);
    if (m) seen.add(`${m[1]}→${m[2]}${m[3] === 'tcp' ? '' : `/${m[3]}`}`);
  }
  return [...seen].join(', ');
}

function Images() {
  const state = store.useStore();
  const images = state.images ?? [];
  if (images.length === 0) return <Text style={{ color: 'muted', padding: 8 }}>No images.</Text>;
  return (
    <>
      {images.map((image) => (
        <ImageRow key={`${image.id}:${image.repository}:${image.tag}`} image={image} />
      ))}
    </>
  );
}

function ImageRow({ image }: { image: Image }) {
  const state = store.useStore();
  const name = image.repository === '<none>' ? `<none> ${image.id}` : `${image.repository}:${image.tag}`;
  const inUse = (state.containers ?? []).some((c) => c.image === name || c.image === image.repository || c.image.startsWith(image.id));
  const key = `image:${image.id}:${name}`;
  const onMenu = ({ id }: { id: string }) => {
    if (id === 'inspect') actions.inspect(image.id, name);
    else if (id === 'copy') forge.clipboard.writeText(image.id);
    else if (id === 'rm') actions.removeImage(image);
  };
  return (
    <TreeItem
      label={name}
      description={`${image.size} · ${image.created}`}
      icon="image"
      iconColor={inUse ? 'accent' : 'muted'}
      loading={state.busy.has(image.id)}
      selected={state.selected === key}
      onClick={() => store.select(key)}
      contextMenu={[
        { id: 'inspect', label: 'Inspect', icon: 'json' },
        { id: 'copy', label: 'Copy ID', icon: 'copy' },
        { separator: true },
        { id: 'rm', label: 'Remove…', icon: 'trash', danger: true },
      ]}
      onContextMenu={onMenu}
    />
  );
}
