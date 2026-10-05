# Architecture

How Forge is put together, for people working on it. [DEVELOPMENT.md](DEVELOPMENT.md) covers building and testing it, [ZED_INTEGRATION.md](ZED_INTEGRATION.md) how it uses Zed, and [EXTENSIONS.md](EXTENSIONS.md) the extension API from the extension's side.

Forge is a native GPUI application assembled from Zed's crates, with Forge's own crates on
top: agent threads over ACP, a React extension host, and IDE features (run, tests, .NET,
HTTP, GitHub, updates) built from Zed's workspace, editor, project and terminal.

```text
forge-native (binary `forge`): bootstrap, menus, title & status bars, settings tab, themes
 ├─ Zed: workspace, editor, project (LSP, git, debugger), terminal_view, git_ui, search…
 │    (+ the small hooks in patches/zed)
 ├─ forge-languages ─── C# (OmniSharp, netcoredbg locators), MSBuild, .sln, .http grammars
 ├─ forge-run ───────── run targets → Run / Debug / hot reload as Zed tasks & debug scenarios
 ├─ forge-tests ─────── Tests panel: discovery and runners per kind; gutter statuses
 ├─ forge-dotnet ────── Solution Explorer, NuGet, project files (over dotnet-model)
 ├─ forge-git ───────── Zed's git UI + history graph, merge editor, git init, background fetch
 ├─ forge-github ────── `gh` CLI → pull request status, picker, review comment blocks
 ├─ forge-http ──────── .http requests through Zed's HTTP client → response editor
 ├─ forge-update ────── GitHub releases → verified app swap
 ├─ forge-output ────── Output panel, language server status
 ├─ forge-agents ────── Thread (model) → ThreadView tab, anchored cards, worktrees (GPUI)
 │     ├─ ProjectFs   : WorkspaceEngine over Zed buffers  ┐  GPUI thread
 │     ├─ ZedTerminals: TerminalHost over Zed terminals   ┘
 │     └─ acp-client (tokio) ── jsonrpc ── agent subprocesses (stdio, ACP)
 └─ forge-extension-host ── Extensions panel, slot panels and tabs (GPUI), extension API
       ├─ QuickJS thread: runtime.js (React + reconciler) + every extension's bundle
       └─ child processes: process.spawn and sidecars (bin/<platform>/ in the extension)
forge-ui: what several of these share (panel drag & drop, pickers, settings registry, processes)
```

## Boundaries

- **`ide-api`** holds the only contract between the ACP runtime and its host: `WorkspaceEngine` (agent file access), `TerminalHost` (agent commands), `AgentService` and events. `acp-client` depends on nothing else, so it is tested without GPUI against `tools/mock-acp-agent.py`.
- **Threading:** ACP runs on tokio (`gpui_tokio`), extensions run on their own QuickJS thread, and everything that touches Zed entities runs on the GPUI thread. The adapters (`ProjectFs`, `ZedTerminals`, `JsHost`) cross these boundaries with channels, so no lock is ever shared with the UI. Slow work (test runs, `git`, `gh`, HTTP, discovery) runs on the background executor and reports back.
- **Agents act through the editor, not around it.** Reads see unsaved buffers. Writes update open buffers and save them. Commands run in Zed terminals that the user watches live. Paths outside the project's worktrees are refused, so a thread in a git worktree works inside `.forge/worktrees/` within the project. Tool calls go through the user's approval.
- **Extensions are trusted code with a narrow UI surface.** They never get a DOM (unless they open a web view panel): they render through the reconciler's op stream. What they can do outside their panels goes through `@forge/api` calls, each implemented explicitly in `forge-extension-host`: files, the active editor, events, settings, storage, keychain secrets, dialogs, and, through `process.*` and `terminal.run`, programs with the user's rights (sidecars included). Everything an extension registers or starts is tracked per extension and undone when it unloads. Install only extensions you trust, as with any editor.
- **External tools over reimplementations.** Tests run with each ecosystem's own tool, GitHub goes through `gh` (its sign-in, no tokens in Forge), .NET through `dotnet`, worktrees through `git`. Forge parses their output and keeps the UI.
- **Changes to Zed stay hooks.** When Forge needs something Zed's public API doesn't offer, a small generic hook goes into `patches/zed` (see `docs/ZED_INTEGRATION.md`) and the feature lives in a Forge crate.

## Where things are

**Agents** (`crates/forge-agents`). `thread.rs` is the model: one ACP session, its entries (messages, thoughts, tool calls, permissions, reviews), checkpoints per turn, the files the agent changed and what it is doing now. `threads.rs` is the tab that shows it, with the follow pane, the changed-files list and the composer; `threads/turns.rs` lays the conversation out turn by turn. Agent file access goes through `project_fs.rs` (Zed buffers, review of writes), commands through `terminals.rs` (Zed terminals). `permissions.rs` answers permission requests by policy and picks the agent's own mode; `rules.rs` sends the user's and the project's instructions; `worktree.rs` gives a thread its own git worktree and merges it back; `anchored.rs` is the card in the code; `conflicts.rs` takes merge conflicts from Zed's git UI.

**Extension host** (`crates/forge-extension-host`). `js.rs` runs QuickJS on its own thread and scopes each bundle's timers; `host.rs` loads extensions and serves API calls, with the editor, events, storage and terminal calls in `api.rs` and processes and sidecars in `process.rs`. `tree.rs` holds each panel's and tab's UI tree, `surface.rs` draws it with Zed's components (down to the data grid), `panel.rs` and `layout.rs` are the dock slots, `tab.rs` the tabs in the editor area, `webview.rs` the web views, `overview.rs` the Extensions panel, and `install.rs` with `package.rs` install, reload, uninstall and pack extensions. The JavaScript side is `packages/forge-api`: `index.ts` (components and API), `reconciler.ts`, `polyfills.ts` and `runtime.ts`.

**Run, tests and debugging.** `forge-run` finds run targets from manifests (`targets.rs`) and drives Run / Debug / Stop / hot reload (`controller.rs`) as Zed tasks and debug scenarios. `forge-tests` discovers tests from source per kind (`discovery.rs`, `go.rs`, `rust.rs`, `node.rs`, `python.rs`), runs them (`runner.rs`) and shows them (`panel.rs`, `gutter.rs`). Debug sessions come from debug locators: Forge's for .NET in `forge-languages/src/netcoredbg.rs`, Zed's for Cargo, Go and Python, and a ready `pwa-node` scenario for Jest and Vitest.

**Everything else** keeps to one crate each: `forge-dotnet` (Solution Explorer and NuGet, over `dotnet-model`), `forge-git` (history graph, merge editor), `forge-github`, `forge-http`, `forge-output`, `forge-update`, and `forge-native`, which assembles the app.

## Zed dependency

Zed is pinned in `vendor/zed` (see `docs/ZED_INTEGRATION.md`) and used as a library. Forge's root `Cargo.toml` is an integration workspace: it copies Zed's `[patch]` sections, dev profiles and `Cargo.lock`, so Zed's crates resolve exactly as they do upstream.
