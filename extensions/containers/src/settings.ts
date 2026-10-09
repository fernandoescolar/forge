// The extension's settings, read at activation and kept current.
import { forge } from '@forge-ide/api';
import type { Disposable } from '@forge-ide/api';
import { setCommand } from './docker';
import { refresh } from './store';

export const settings = { shell: 'sh', logLines: 500, confirmRemove: true };

export async function load(): Promise<Disposable> {
  const [command, shell, logLines, confirmRemove] = await Promise.all([
    forge.settings.get<string>('containers.command'),
    forge.settings.get<string>('containers.shell'),
    forge.settings.get<number>('containers.logLines'),
    forge.settings.get<boolean>('containers.confirmRemove'),
  ]);
  setCommand(command);
  settings.shell = shell?.trim() || 'sh';
  settings.logLines = logLines ?? 500;
  settings.confirmRemove = confirmRemove ?? true;
  return forge.settings.onDidChange((key, value) => {
    if (key === 'containers.command') {
      setCommand(value as string | null);
      refresh();
    } else if (key === 'containers.shell') settings.shell = (value as string | null)?.trim() || 'sh';
    else if (key === 'containers.logLines') settings.logLines = (value as number | null) ?? 500;
    else if (key === 'containers.confirmRemove') settings.confirmRemove = (value as boolean | null) ?? true;
  });
}

/** Asks before something that can't be undone (unless the user turned that off). */
export async function confirmed(message: string, detail: string, button: string): Promise<boolean> {
  if (!settings.confirmRemove) return true;
  return (await forge.window.confirm(message, { detail, buttons: [button, 'Cancel'], level: 'danger' })) === 0;
}
