// Database Explorer: browse SQL Server, PostgreSQL, MySQL/MariaDB and SQLite databases,
// open tables and views, edit their rows and run your own queries; MongoDB's collections,
// finding, aggregating and editing their documents; and Redis's keys (of every type, with
// TTLs) and a console.
//
// The databases are reached through `forge-sql` (sidecar/), a program shipped with the
// extension in bin/<platform>/ and started with `forge.process.sidecar`.
import { forge } from '@forge/api';
import type { ExtensionContext } from '@forge/api';
import { sql } from './client';
import { Explorer } from './explorer';
import * as store from './store';
import { openConnectionForm, openQuery } from './tabs';

export async function activate(ctx: ExtensionContext) {
  await store.init(ctx);
  store.onNeedPassword((id) => openConnectionForm(id, 'Type the password to connect (it is not saved).'));

  ctx.subscriptions.push(
    forge.panels.register({ id: 'db-explorer', title: 'Databases', icon: 'database_zap', layout: 'fill', render: () => <Explorer /> }),
    forge.commands.register('db-explorer.addConnection', 'Add Connection', () => openConnectionForm(null)),
    forge.commands.register('db-explorer.newQuery', 'New Query', () => {
      const target = store.currentTarget();
      if (!target) return forge.window.showMessage('Add a connection first.', 'warning');
      openQuery(target.connectionId, target.database);
    }),
    forge.commands.register('db-explorer.runFile', 'Open the Active File (or Selection) in a Query', async () => {
      const editor = await forge.editor.active();
      if (!editor) return forge.window.showMessage('Open a .sql file first.', 'warning');
      const text = editor.selectedText || (await forge.editor.getText()) || '';
      const target = store.currentTarget();
      if (!target) return forge.window.showMessage('Add a connection first.', 'warning');
      openQuery(target.connectionId, target.database, text);
    }),
  );
}

export function deactivate() {
  sql.stop();
}
