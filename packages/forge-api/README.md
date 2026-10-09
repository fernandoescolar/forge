# @forge-ide/api

The API for [Forge](https://github.com/fernandoescolar/forge) extensions, and `forge-ext`, the tool that builds and packs them.

Forge extensions are TypeScript and React. There is no DOM: your components render as native GPUI elements, from simple panels to a virtualized, editable data grid, Markdown, images and charts. The API reaches the workspace, the active editor, events, processes and bundled sidecars, terminals, settings, storage, keychain secrets, native dialogs and tabs in the editor area, and offers agents tools of your own (`forge.agents.registerTool`) through Forge's MCP server.

```bash
npm install --save-dev @forge-ide/api
```

```tsx
import { forge, Text } from '@forge-ide/api';
import type { ExtensionContext } from '@forge-ide/api';

export function activate(ctx: ExtensionContext) {
  ctx.subscriptions.push(forge.panels.register({ id: 'hello', title: 'Hello', render: () => <Text>Hi</Text> }));
}
```

```bash
npx forge-ext build .    # or `watch`
npx forge-ext pack .     # → <name>-<version>.forgeext
```

React and the API aren't bundled into your extension: Forge provides them at run time, so this package holds the types and `forge-ext`. For type checking, add `"types": ["@forge-ide/api/globals"]` to your `tsconfig.json` (the timers and `console` the runtime provides).

The guide, from an empty folder to a packaged extension: [Writing extensions](https://github.com/fernandoescolar/forge/blob/main/docs/EXTENSIONS.md).

Before this package was published, the API was called `@forge/api`; extensions built with that name keep working.

0.2.0 adds the `Markdown`, `Image` and `Chart` components and removes `forge.webviews`: extensions render only with native components.
