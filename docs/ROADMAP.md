# Roadmap

## Done
- Native GPUI shell on Zed: editor, LSP, terminal, project panel, finder, palette, search
- Palette themes (Dracula, Forge Dark) with hot reload; Forge default settings
- React extension host: QuickJS runtime, reconciler → GPUI, Extensions panel, workspace API
- ACP agent panel: chat (markdown), thoughts, tool calls, plans, permissions, cancel
- Agent file access through Zed buffers; agent commands in embedded Zed terminals
- Agent edits as syntax-highlighted diffs; write review gate (accept/reject)
- Syntax highlighting wired to palette themes (regression-tested)
- Agents start with the project's login-shell environment
- `@file` mentions with completion; active file/selection attached to prompts
- Conversation history per project; resume with `session/load`
- Per-hunk partial acceptance of agent edits; open diffs in the editor
- Extension commands in the command palette and bindable as actions
- Webview panels (wry) for DOM-based extension UIs
- macOS app bundle (icon, Finder launch/open-with, bundled extensions, ad-hoc signing)
- MCP servers from agents.json passed to every agent session
- Drag & drop of panels (status-bar icons, panel grips) to window-edge drop zones; extension panels as independent slots with draggable, groupable tabs; layout persisted
- In-thread agent sign-in (ACP terminal auth / authenticate), restart + retry
- Threads replace the agent panel: conversation as a document in the editor area,
  editable proposals, threads anchored in code (ctrl-enter), following the agent live
- Full application menu bar (every item backed by a handler, tested); Forge welcome page with recent projects; Forge logo replaces Zed's
- Run targets for `package.json` scripts and Python programs; pytest in the Tests panel
- Extension API: editor (state, edits, selections, decorations), events, `process.exec`, `terminal.run`, storage; install from folder
- Open Recent picker; your and the project's instructions (`.forge/AGENTS.md`) sent to agents; .NET launch profiles as run targets (run and debug); `.http` files with environments, named responses and dynamic variables
- Threads in their own git worktree (apply to the project, remove; Agents › Worktrees…); extension reload and uninstall without restarting; decorations that follow a file; `.http` highlighting and run buttons
- Merge conflicts resolved by agents (Zed's inline conflict buttons + Forge threads); GitHub pull requests through `gh`: status bar with checks, list, checkout, review with an agent, review comments in the code
- .NET: hot reload (`dotnet watch`) from the title bar; user secrets and EF Core migrations from the Solution Explorer; debug tests from the Tests panel (.NET, Go, Rust, Python)
- Distribution: CI and release workflows (tests; app for Apple silicon and Intel with precompiled shaders, signed and notarized when the secrets are set); notarization in `bundle-macos.sh`; self-update from GitHub releases
- Extension packages (`.forgeext`, with sidecars per platform): install, export, `forge-ext pack`; extension tabs in the editor area; native data grid, tree items, selects, context menus; `process.spawn` and sidecars, keychain secrets, dialogs, clipboard; Database Explorer (SQL Server, PostgreSQL, MySQL/MariaDB, SQLite) with the `forge-sql` sidecar
- Known gaps closed: debugging Jest and Vitest tests; `.http` streaming and binary responses, cancel, request history; extension timers stop on unload, decorations follow edits; thread worktrees merge with your uncommitted changes; a three-pane merge editor

## Next

The planned work is done, and so are the gaps found along the way. What is left needs
things outside the code.

### Outside the code
- Push to `github.com/fernandoescolar/forge`, so CI and the release workflow run (the
  workflows are ready and pass `actionlint`, but have never run on GitHub).
- An Apple Developer ID certificate and an app-specific password, as the release workflow's
  secrets, for signed and notarized builds. Until then releases are ad-hoc signed and the
  hardened-runtime entitlements (`scripts/Forge.entitlements`, empty) are untested.
