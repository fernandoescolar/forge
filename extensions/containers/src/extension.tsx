// Containers: Docker's containers (by Compose project) and images in a panel, with their
// logs and a shell in Forge terminals. Everything goes through the `docker` CLI (or the one
// set in Containers › Docker command), so whatever runs the engine (Docker Desktop,
// OrbStack, Colima, Podman) works. Agents can use them too (agentTools.ts).
import { forge } from '@forge-ide/api';
import type { ExtensionContext } from '@forge-ide/api';
import * as actions from './actions';
import { registerAgentTools } from './agentTools';
import { Panel } from './panel';
import { load } from './settings';
import * as store from './store';

export async function activate(ctx: ExtensionContext) {
  ctx.subscriptions.push(await load());
  store.init();

  // Agents can see the containers and their logs, and start or restart them when the user says yes.
  ctx.subscriptions.push(...registerAgentTools());
  ctx.subscriptions.push(
    forge.panels.register({ id: 'containers', title: 'Containers', icon: 'box', layout: 'fill', render: () => <Panel /> }),
    forge.commands.register('containers.refresh', 'Refresh Containers', () => store.refresh()),
    forge.commands.register('containers.composeUp', 'Compose Up (the Active Compose File or the Project’s)', () => actions.composeUp()),
  );
}

export function deactivate() {
  store.stop();
}
