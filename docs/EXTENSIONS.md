# Writing Forge extensions

Forge extensions are written in TypeScript with React, and Forge draws them with its own native UI: no web view, no DOM, no CSS. An extension can add panels, tabs in the editor area, commands and settings. It can also read and change the code in the active editor, run programs (including ones it ships, called *sidecars*) and keep data and secrets. When an extension needs the DOM (a chart library, an existing web app), it can open a web view panel instead.

This guide goes from an empty folder to a packaged extension, then covers the components and the API. Three complete extensions live in this repository's `extensions/` folder:

| Example | What it shows |
| --- | --- |
| `workspace-notes` | A native panel with state, settings and the workspace API: the place to start |
| `webview-demo` | A web view panel exchanging messages with its extension |
| `db-explorer` | Database Explorer, which ships with Forge: a tree, tabs in the editor area, an editable data grid, a Rust sidecar, keychain secrets and dialogs |

- [Your first extension](#your-first-extension)
- [The manifest](#the-manifest)
- [Activation and the context](#activation-and-the-context)
- [Components](#components)
- [Panels, tabs, commands and web views](#panels-tabs-commands-and-web-views)
- [Working with the editor and the workspace](#working-with-the-editor-and-the-workspace)
- [Programs and sidecars](#programs-and-sidecars)
- [Settings, storage and secrets](#settings-storage-and-secrets)
- [Dialogs, messages and the clipboard](#dialogs-messages-and-the-clipboard)
- [Packaging and sharing](#packaging-and-sharing)
- [Debugging](#debugging)
- [API reference](#api-reference)
- [How it works](#how-it-works)

## Your first extension

The API package and the build tool live in this repository (`packages/forge-api`), so start from a checkout of Forge. The examples in `extensions/` show the layout.

**1. Create the folder** `extensions/hello/` with a `package.json`:

```json
{
  "name": "hello",
  "displayName": "Hello",
  "version": "0.1.0",
  "description": "My first Forge extension",
  "forge": { "entry": "src/extension.tsx" }
}
```

**2. Write `src/extension.tsx`:**

```tsx
import { useState } from 'react';
import { forge, Button, Text, View } from '@forge/api';
import type { ExtensionContext } from '@forge/api';

function Hello() {
  const [clicks, setClicks] = useState(0);
  return (
    <View style={{ gap: 8 }}>
      <Text style={{ weight: 'bold', size: 'lg' }}>Hello from React</Text>
      <Button label={`Clicked ${clicks} times`} icon="sparkle" onClick={() => setClicks(clicks + 1)} />
    </View>
  );
}

export function activate(ctx: ExtensionContext) {
  ctx.subscriptions.push(
    forge.panels.register({ id: 'hello', title: 'Hello', icon: 'sparkle', render: () => <Hello /> }),
    forge.commands.register('hello.greet', 'Say Hello', () => forge.window.showMessage('Hello!')),
  );
}
```

**3. Build it:**

```bash
node packages/forge-api/bin/forge-ext.mjs build extensions/hello    # or `watch` to rebuild on every change
```

This bundles your code into `extensions/hello/dist/extension.js`. React and `@forge/api` are not bundled: Forge provides them, so every extension shares the same React.

**4. Run it.** A debug build of Forge (`cargo run -p forge-native`) loads everything in `extensions/` at startup. With an installed Forge, use **Extensions › Install from Folder…** and pick the folder. After a rebuild, *Reload* in the extension's ⋯ menu (Extensions panel) loads it again, without restarting Forge.

Your panel shows under View › Panels and in the dock, and *Hello: Say Hello* is in the command palette.

For type checking, copy `tsconfig.json` from `extensions/workspace-notes`. It points `@forge/api` and React's types at `packages/forge-api`.

## The manifest

An extension is a folder whose `package.json` has a `forge` section:

| Field | Meaning |
| --- | --- |
| `name` | The extension's id. Prefix your panel, tab and command ids with it. |
| `displayName`, `version`, `description` | Shown in the Extensions panel |
| `forge.entry` | The source file `forge-ext` bundles (`.ts` or `.tsx`) |
| `forge.main` | The bundle Forge loads; defaults to `dist/extension.js` |
| `forge.settings` | Settings the extension declares, as a JSON schema (see [Settings](#settings-storage-and-secrets)) |
| `forge.sidecars` | Names of the programs it ships in `bin/<platform>/` (see [Sidecars](#programs-and-sidecars)) |
| `forge.files` | The files a package includes, if the defaults don't suit (see [Packaging](#packaging-and-sharing)) |

Forge looks for extensions in `~/Library/Application Support/Forge/extensions` (where installing puts them), in the app bundle (the ones that ship with Forge), in the folders listed in `$FORGE_EXTENSIONS_PATH` (colon-separated), and in debug builds in this repository's `extensions/`.

## Activation and the context

The bundle exports `activate(ctx)`, called once when Forge loads it, and optionally `deactivate()`. `activate` may be `async`.

```ts
export async function activate(ctx: ExtensionContext) {
  const count = (await ctx.storage.get<number>('launches')) ?? 0;
  await ctx.storage.set('launches', count + 1);
}

export function deactivate() {
  // stop what isn't a Disposable in ctx.subscriptions
}
```

`ctx` holds:

| Member | What it is |
| --- | --- |
| `id`, `path` | The extension's name and folder |
| `subscriptions` | `Disposable`s to dispose when the extension unloads |
| `storage` | JSON values kept for this extension across projects |
| `workspaceStorage` | JSON values kept for this extension in the current project |
| `secrets` | Strings kept in the macOS keychain |

You don't strictly need `subscriptions`: whatever an extension registers through `forge` (panels, tabs, commands, listeners), the processes it starts and the timers it sets are all undone when it unloads, whether it is reloaded, uninstalled or Forge quits.

## Components

Components are React elements that Forge draws natively, with the active theme's colours and fonts.

| Component | Props |
| --- | --- |
| `View` | A flex container. `onClick`, `tooltip`, `hidden` |
| `Scroll` | A `View` that scrolls its content |
| `Text` | Text; `selectable` |
| `Button` | `label`, `icon`, `variant` (`filled`, `subtle`, `ghost`), `disabled`, `selected`, `tooltip`, `onClick` |
| `Input` | `value`, `placeholder`, `onChange`, `onSubmit` (Enter, or ⌘Enter when `multiline`), `multiline`, `password`, `autoFocus`, `language` (highlights the text as that language, by Forge's name for it: `"JSON"`, `"YAML"`…) |
| `Checkbox` | `checked`, `label`, `onChange` |
| `Select` | A drop-down: `value`, `options` (`{ value, label }`), `placeholder`, `disabled`, `onChange` |
| `Icon` | `name`: one of Zed's icon names, such as `sparkle`, `file_tree`, `database_zap` |
| `Divider` | `direction` (`horizontal`, `vertical`) |
| `Spinner` | A turning progress icon |
| `TreeItem` | One row of a tree: `label`, `description`, `icon`, `iconColor`, `depth`, `expanded` (shows the disclosure arrow), `selected`, `loading`, `onToggle`, `onClick`, `onDoubleClick`. You render the visible rows in order. |
| `Tabs` | A tab bar: `tabs` (`{ id, label, icon?, closable? }`), `active`, `onSelect`, `onClose` |
| `DataGrid` | A table (see below) |

**Style.** Every component takes `style`:

- **Layout:** `direction` (`row`, `column`), `gap`, `padding`, `paddingX`, `paddingY`, `align`, `justify`, `grow`, `shrink`, `wrap`, `width` / `height` (pixels or `'full'`), `minWidth`, `maxWidth`.
- **Colour:** `background` and `color` take theme tokens: `default`, `muted`, `accent`, `error`, `warning`, `success`, `surface`, `elevated`, `panel`, `editor`, `transparent`.
- **Borders:** `border`, `borderSide` (`top`, `bottom`, `left`, `right`), `rounded`.
- **Text:** `size` (`xs`, `sm`, `md`, `lg`), `weight` (`normal`, `medium`, `bold`), `mono`, `italic`, `truncate`.

**Context menus.** Any component takes `contextMenu`, shown on right click. It's a list of items `{ id, label, icon?, disabled?, danger? }`, `{ separator: true }` and `{ header }`. The choice comes back in `onContextMenu({ id })`.

```tsx
<TreeItem
  label="users"
  icon="table"
  depth={2}
  contextMenu={[{ id: 'open', label: 'Open Rows' }, { separator: true }, { id: 'drop', label: 'Drop Table…', danger: true }]}
  onContextMenu={({ id }) => run(id)}
/>
```

**DataGrid** is Zed's table:

- **Fast with large data:** rows are virtualized, so thousands are fine (only the visible ones are drawn). Columns are resizable, the first column numbers the rows, and the grid scrolls both ways.
- **Selection:** click, ⌘-click and shift-click select rows (`onSelect`), ⌘C copies them as tab-separated text, and Delete calls `onDeleteRows`.
- **Sorting:** a header click calls `onSort`.
- **Editing:** with `editable`, a double-click edits a cell in place (`onCellEdit`); without it, a double-click calls `onRowActivate`.
- **Pending changes:** `rowStates` (`new`, `modified`, `deleted`) and `editedCells` (`"row:column"`) colour rows and cells that haven't been saved.

Give it room with `style: { grow: true }`.

```tsx
<DataGrid
  style={{ grow: true }}
  columns={[{ name: 'id', type: 'int4', primaryKey: true }, { name: 'name', type: 'text' }]}
  rows={[[1, 'Ada'], [2, null]]}
  selectedRows={selected}
  onSelect={setSelected}
  editable
  onCellEdit={({ row, column, value }) => edit(row, column, value)}
/>
```

## Panels, tabs, commands and web views

**Panels** sit in the docks. Users drag them to any side, group them with other panels, and find them under View › Panels.

```ts
forge.panels.register({
  id: 'hello',            // unique across extensions: prefix it
  title: 'Hello',
  icon: 'sparkle',        // a Zed icon name
  position: 'right',      // where it starts: left, right or bottom
  layout: 'scroll',       // 'fill' when your content has its own scrolling parts (a tree and a grid)
  render: () => <Hello />,
});
```

**Tabs** open in the editor area, next to the files. That's the place for documents: a table's rows, a query, a report. `open` shows the existing tab with that id rather than opening a second one.

```ts
const tab = forge.tabs.open({ id: 'hello.report', title: 'Report', icon: 'file', render: () => <Report />, onClose: () => cleanUp() });
tab.setTitle('Report (3 issues)');
tab.update({ icon: 'warning', tooltip: '3 issues found' });
tab.close();
```

**Commands** appear in the command palette as *Extension name: Title* and under the Extensions menu. They can be bound to keys in `keymap.json`:

```ts
forge.commands.register('hello.greet', 'Say Hello', () => forge.window.showMessage('Hello!'));
```

```json
{ "bindings": { "cmd-alt-h": ["forge_extensions::RunCommand", { "id": "hello.greet" }] } }
```

**Web view panels** run a page in the system web view (WebKit). It's served from your extension's folder over `forge-ext://`, gets the theme as CSS variables (`--forge-bg`, `--forge-fg`, `--forge-muted`, `--forge-accent`, `--forge-border`, `--forge-surface`), and talks to the extension in messages:

```ts
// extension
const panel = forge.webviews.register({ id: 'demo', title: 'Demo', icon: 'globe', html: 'webview/index.html' });
panel.onMessage((m) => panel.postMessage({ echo: m }));
```

```html
<!-- webview/index.html -->
<script>
  window.forge.onMessage((m) => console.log(m));
  window.forge.postMessage({ hello: true });
</script>
```

The web view is a native view on top of the panel, so Forge popovers that overlap the panel draw underneath it. During development, `FORGE_EXTENSIONS_PANEL=<panel id>` opens the Extensions panel on that tab at startup. `extensions/webview-demo` is the complete example.

## Working with the editor and the workspace

**The workspace:**

```ts
const roots = await forge.workspace.roots();               // the project's folders
const text = await forge.workspace.readFile('src/main.rs'); // with unsaved changes, when it's open
await forge.workspace.openFile('src/main.rs', 41);          // zero-based line
const active = await forge.workspace.activeFile();          // path or null
forge.workspace.onDidChangeActiveFile((path) => { /* … */ });
forge.workspace.onDidSaveFile((path) => { /* … */ });
```

Relative paths resolve against the first project folder.

**The active editor.** Positions are `{ line, column }`, zero-based, with columns in characters:

```ts
const editor = await forge.editor.active();
// { path, language, lineCount, selections: [{ start, end, reversed }], selectedText } or null

await forge.editor.replaceSelections(editor.selectedText.toUpperCase());
await forge.editor.edit([{ range: { start: { line: 0, column: 0 }, end: { line: 0, column: 0 } }, text: '// header\n' }]);
await forge.editor.select({ start: { line: 3, column: 0 }, end: { line: 3, column: 10 } });
const all = await forge.editor.getText();
forge.editor.onDidChangeSelection((state) => { /* … */ });
```

`edit` applies all its edits as one undo step, and its ranges refer to the text before the edits.

**Decorations** highlight ranges of a file. They show in every editor of that file, including ones opened later, and they move with the text as it's edited:

```ts
await forge.editor.setDecorations('hello.todos', ranges, 'warning');           // the active file
await forge.editor.setDecorations('hello.todos', ranges, 'warning', 'src/a.rs'); // another file
await forge.editor.setDecorations('hello.todos', []);                           // clear them
```

Colours: `accent`, `error`, `warning`, `success`, `muted`. Forge only sends the events some extension listens to, so listening costs nothing until you subscribe.

## Programs and sidecars

**Run a program to completion** with the project's shell environment (so `PATH` finds the user's tools):

```ts
const { code, stdout, stderr } = await forge.process.exec('git', { args: ['log', '--oneline', '-5'], cwd: '.', env: { GIT_PAGER: 'cat' } });
```

**Keep one running** and talk to it:

```ts
const child = await forge.process.spawn('python3', { args: ['-u', 'server.py'] });
child.onLine((line) => handle(JSON.parse(line)));   // standard output, a line at a time
child.onStderr((text) => console.warn(text));
child.write(JSON.stringify({ op: 'ping' }) + '\n');
child.onExit((code) => console.log('exited', code));
// child.end() closes its input; child.kill() stops it; await child.exited
```

**Sidecars** are programs your extension ships, built for each platform. List them in `package.json`, and put each build at `bin/<platform>/<name>`, where the platform is `darwin-arm64`, `darwin-x64`, `linux-x64`, `win32-x64`…:

```json
"forge": { "entry": "src/extension.tsx", "sidecars": ["my-tool"] }
```

```ts
const tool = await forge.process.sidecar('my-tool', { args: ['--serve'] });
```

Database Explorer works this way. Its Rust program `forge-sql` (`extensions/db-explorer/sidecar`) speaks JSON lines over standard input and output, and `src/client.ts` matches requests to answers. The extension's processes are killed when it unloads.

**Terminals.** `forge.terminal.run(command, { args, cwd, title })` runs a command in a terminal tab, like a task, where the user can watch it and type into it.

## Settings, storage and secrets

**Settings** are declared in the manifest as a JSON schema. They get a page in Forge › Settings and are stored in `config/extensions.json`:

```json
"forge": {
  "settings": {
    "title": "Hello",
    "properties": {
      "hello.greeting": { "type": "string", "default": "Hello", "description": "What the command says." },
      "hello.loud": { "type": "boolean", "default": false, "title": "Shout" }
    }
  }
}
```

```ts
const greeting = await forge.settings.get<string>('hello.greeting');  // the user's value, else the default
forge.settings.onDidChange((key, value) => { /* value is null once reset */ });
```

Supported: `type` (`string`, `boolean`, `integer`, `number`, `object`, `array`), `default`, `title`, `description`, `enum` with `enumDescriptions`, and `minimum` / `maximum`.

**Storage** keeps JSON values between sessions, per extension (`ctx.storage`) or per extension and project (`ctx.workspaceStorage`): `get(key)`, `set(key, value)` (`null` removes it), `keys()`.

**Secrets** keep strings in the macOS keychain, per extension, for passwords and tokens: `ctx.secrets.get(key)`, `set(key, value)`, `delete(key)`.

## Dialogs, messages and the clipboard

```ts
forge.window.showMessage('Saved', 'info');                                   // or 'warning', 'error'
const choice = await forge.window.confirm('Delete 3 rows?', { detail: 'This cannot be undone.', buttons: ['Delete', 'Cancel'], level: 'danger' });
const files = await forge.window.pickFiles({ files: true, multiple: true });  // null when cancelled
const target = await forge.window.saveFile({ name: 'export.csv' });
await forge.clipboard.writeText('copied');
const pasted = await forge.clipboard.readText();
```

## Packaging and sharing

A `.forgeext` file is a zip of what an extension needs at run time: `package.json`, `dist/`, its web view pages and assets, and its sidecars for every platform they were built for. Sources and `node_modules` stay out. By default a package takes `package.json`, `dist`, `webview`, `assets`, `media`, `bin`, `README.md`, `CHANGELOG.md`, `LICENSE` and `icon.png`; list `forge.files` in the manifest to choose yourself.

```bash
node packages/forge-api/bin/forge-ext.mjs pack extensions/hello          # → hello-0.1.0.forgeext
```

*Export as Package…* in an extension's ⋯ menu (Extensions panel) does the same from Forge.

To install a package, use **Extensions › Install from Package…** (or *Install* in the Extensions panel). Forge checks it is a built extension with its sidecars for this Mac, unpacks it into `~/Library/Application Support/Forge/extensions/<name>`, keeping Unix permissions so sidecars stay executable, and loads it, replacing an installed copy.

Extensions run with the user's rights: they can read files and run programs. Say what yours does in its description.

## Debugging

- **Logs.** `console.log`, `console.warn` and `console.error` (and `forge.log`) go to Forge's log, in the Output panel. Errors also show on the extension's card in the Extensions panel.
- **Reloading.** Run `forge-ext watch` and *Reload* the extension after each change, or keep a debug build of Forge running from the repository.
- **Timers.** `setTimeout`, `setInterval` and their `clear…` functions work as in a browser; each extension's timers stop when it unloads. There is no `fetch` or DOM: talk to the network from a sidecar or with `forge.process`.

## API reference

| Namespace | Members |
| --- | --- |
| `forge.panels` | `register({ id, title, icon, position, layout, render })` |
| `forge.tabs` | `open({ id, title, icon, render, onClose })` → `{ setTitle, update, close }` |
| `forge.webviews` | `register({ id, title, icon, html })` → `{ postMessage, onMessage }` |
| `forge.commands` | `register(id, title, handler)` |
| `forge.workspace` | `roots`, `readFile`, `openFile`, `activeFile`, `onDidChangeActiveFile`, `onDidSaveFile` |
| `forge.editor` | `active`, `getText`, `edit`, `replaceSelections`, `select`, `setDecorations`, `onDidChangeSelection` |
| `forge.process` | `exec`, `spawn`, `sidecar` |
| `forge.terminal` | `run` |
| `forge.settings` | `get`, `onDidChange` |
| `forge.window` | `showMessage`, `confirm`, `pickFiles`, `saveFile` |
| `forge.clipboard` | `writeText`, `readText` |
| `ctx` | `id`, `path`, `subscriptions`, `storage`, `workspaceStorage`, `secrets` |

Everything returning a `Disposable` (`{ dispose() }`) can go into `ctx.subscriptions`. The types are in `packages/forge-api/src/index.ts`, with a comment on each.

## How it works

1. `packages/forge-api` bundles React 19, a custom `react-reconciler` host config and the API into `dist/runtime.js`, which Forge embeds.
2. Every extension runs in one QuickJS runtime, on a thread of its own, so JavaScript never runs on the UI thread and a busy extension can't freeze the editor.
3. Each React commit becomes a batch of operations (`create`, `append`, `update`, `remove`…) sent to Forge, which keeps a tree per panel or tab and draws it with Zed's UI components.
4. Clicks, typing and other events go back to the QuickJS thread, where your handlers run. API calls cross the same way, as JSON, and resolve your promises.

[ARCHITECTURE.md](ARCHITECTURE.md) shows where the extension host sits in Forge.
