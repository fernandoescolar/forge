# Forge

**The opinionated IDE for real-world .NET, Go, Rust and JavaScript development. Agents built in, built on Zed.**

![Forge: an agent thread that fixed a failing test, next to the diff of its changes](docs/images/agents.jpg)

Forge is a native, GPU-rendered code editor that makes the decisions for you. Open a .NET solution, a Go module, a Cargo workspace or a Node package and it already knows how to run it, test it, debug it and talk about it with an AI agent. There are no extensions to hunt for and no `launch.json` to write first. It is built from the open-source crates of the [Zed](https://zed.dev) editor, so it starts in a blink and stays fast on large projects.

## For developers who use it

- **Your stack, ready on day one.** C# with OmniSharp and a Visual Studio–style Solution Explorer (with NuGet, user secrets and EF Core migrations), Rust, Go, TypeScript/JavaScript and Python with their language servers. Run targets are found from your manifests: .NET apps and launch profiles, Cargo binaries, Go `main` packages, `package.json` scripts and Python programs.
- **Run, test, debug.** One Run / Debug / Stop in the title bar, and hot reload for .NET. The Tests panel covers .NET, Go, Rust, Jest, Vitest and pytest: run or debug any test, with results in the gutter.
- **Agents in the flow, not in a chat box.** Any [ACP](https://agentclientprotocol.com) agent (Claude Code, Codex, Gemini, GitHub Copilot…) works in *threads*, conversations laid out as documents in the editor area. The agent edits through your editor, and you keep or undo every change. Threads can be anchored to a line, follow the agent live, run in their own git worktree, and take your project's standing instructions. Agents are one click away from an error, a failing test, the terminal output, a commit message, a merge conflict or a pull request review.
- **Git and GitHub.** Zed's Git panel, a commit graph, a three-pane merge editor, and your branch's pull request with its checks in the status bar, through `gh`.
- **The rest of a working day.** `.http` request files with environments, a database explorer (SQL Server, PostgreSQL, MySQL/MariaDB, SQLite, MongoDB, Redis), an Output panel, layouts you drag into place, and palette themes you edit in 30 colours.

| | | |
| --- | --- | --- |
| [![Solution Explorer](docs/images/solution-explorer.jpg)](docs/images/solution-explorer.jpg)<br>Solution Explorer | [![Tests](docs/images/tests.jpg)](docs/images/tests.jpg)<br>Tests | [![Debugging](docs/images/debug.jpg)](docs/images/debug.jpg)<br>Debugging |
| [![Terminal](docs/images/terminal.jpg)](docs/images/terminal.jpg)<br>Terminal | [![Database Explorer](docs/images/database-explorer.jpg)](docs/images/database-explorer.jpg)<br>Database Explorer | [![Panels shown together](docs/images/panel-groups.jpg)](docs/images/panel-groups.jpg)<br>Panels shown together |

## For developers who extend it

- **Extensions in TypeScript and React, rendered natively.** No web view and no DOM: your components become GPUI elements, from simple panels to a virtualized, editable data grid, Markdown, images and charts.
- **A real API.** The workspace, the active editor (edits, selections, decorations), events, processes and bundled *sidecar* programs, terminals, settings, storage, keychain secrets, native dialogs, and tabs in the editor area.
- **Shareable.** An extension packs into a `.forgeext` file and installs, reloads and uninstalls without restarting Forge.

## Get Forge

Forge runs on macOS 13 or later (Apple silicon and Intel), on Linux (x86_64 and ARM64, glibc 2.35 or later, Wayland or X11) and, in preview, on Windows (x86_64). Each installer downloads the latest [release](https://github.com/fernandoescolar/forge/releases), checks it, installs it and adds a `forge` command (`forge .` opens the current folder, in the Forge that is running if there is one). Run it again to update by hand; release builds also update themselves.

**macOS and Linux**, in a terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.sh | bash
```

On macOS it checks the app's signature and puts Forge.app in Applications. On Linux it checks the tarball's SHA-256, unpacks it into `~/.local/opt/forge` and adds Forge, with its icon, to your desktop's applications.

**Windows**, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.ps1 | iex
```

It checks the zip's SHA-256, unpacks it into `%LOCALAPPDATA%\Programs\Forge`, adds Forge to the Start menu and the `forge` command to your PATH. Forge isn't code-signed on Windows yet: SmartScreen may warn the first time it opens.

To install a given version, give it to the installer: `curl -fsSL …/install.sh | FORGE_VERSION=0.0.2 bash`, or in PowerShell `$env:FORGE_VERSION = '0.0.2'` before running it. You can also download the release's files yourself: `Forge-<version>-macos-<arch>.zip` for macOS, `Forge-<version>-linux-<arch>.tar.gz` and `Forge-<version>-windows-x86_64.zip`; the [guide](docs/GUIDE.md#install-and-update) explains each.

To build it yourself:

```bash
git clone --recurse-submodules --shallow-submodules https://github.com/fernandoescolar/forge.git forge-ide
cd forge-ide
scripts/apply-zed-patches.sh
cargo run -p forge-native -- /path/to/your/project
```

You need Rust (stable), Node 20+, `cmake` and Python 3. [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) has the details.

## Documentation

| | |
| --- | --- |
| [**User guide**](docs/GUIDE.md) | Everything Forge does and how to use it: layout, settings, themes, .NET, run and test, agents, Git and GitHub, HTTP requests, extensions. Also in the app under Help › Forge Guide. |
| [**Writing extensions**](docs/EXTENSIONS.md) | From an empty folder to a packaged extension: components, the API, sidecars, packaging, with examples. |
| [**Architecture**](docs/ARCHITECTURE.md) | How Forge is put together: its crates, threads, the agent runtime and the extension host. |
| [**Zed integration**](docs/ZED_INTEGRATION.md) | How Forge uses Zed as a library, the few patches it carries and how to move to a new Zed release. |
| [**Development**](docs/DEVELOPMENT.md) | Building, testing, packaging, releases and the repository layout. |

## License

Forge is free software under the GPL-3.0-or-later, as Zed's GPL crates (`editor`, `workspace`, `terminal_view`…) require. It is a separate product from Zed: it has its own app, settings, data and project folder (`.forge/`), and it uses neither Zed's services nor its AI features.
