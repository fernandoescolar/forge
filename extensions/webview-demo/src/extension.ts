import { forge } from '@forge-ide/api';
import type { ExtensionContext } from '@forge-ide/api';

type FromPage = { type: 'hello' } | { type: 'open'; path: string };

export function activate(ctx: ExtensionContext) {
  const panel = forge.webviews.register({ id: 'webview-demo', title: 'Webview', icon: 'globe', html: 'webview/index.html' });

  const sendState = async () => {
    panel.postMessage({ type: 'state', roots: await forge.workspace.roots(), activeFile: await forge.workspace.activeFile() });
  };

  ctx.subscriptions.push(
    panel,
    panel.onMessage((message) => {
      const m = message as FromPage;
      if (m.type === 'hello') sendState().catch((e) => console.error(e));
      if (m.type === 'open') forge.workspace.openFile(m.path).catch((e) => forge.window.showMessage(String(e), 'error'));
    }),
    forge.commands.register('webview-demo.refresh', 'Refresh webview', () => sendState()),
  );
}
