# Developing Forge

How to build, run, test and ship Forge itself. To write extensions instead, see [EXTENSIONS.md](EXTENSIONS.md); for how the pieces fit, [ARCHITECTURE.md](ARCHITECTURE.md).

## Build and run

You need Rust (stable), Node 20+ (to bundle the extension runtime), `cmake`, and Python 3 (only for the mock agent the tests use). Full Xcode is **not** needed for development: the `runtime-shaders` feature compiles Metal shaders at startup.

Zed is a **git submodule** at `vendor/zed`, pinned to a release tag (currently `v1.22.0`) and cloned shallowly:

```bash
git clone --recurse-submodules --shallow-submodules https://github.com/fernandoescolar/forge.git forge-ide
cd forge-ide
# in an existing checkout instead:
git submodule update --init --depth 1
scripts/apply-zed-patches.sh        # Forge's small changes to Zed, see patches/zed

cargo run -p forge-native -- /path/to/project [files...]
```

The first build compiles a large part of Zed and takes a while; later builds are incremental. Debug builds also load the example extensions in this repository's `extensions/`, and offer two test agents in threads: `mock` (`tools/mock-acp-agent.py`) and `mock-auth`, which asks you to sign in first.

A debug build and an installed Forge.app share the same data folder and bundle id. Close one before you run the other.

## Where Forge keeps things

Forge keeps its state in `~/Library/Application Support/Forge`, apart from an installed Zed:

| Path | What it holds |
| --- | --- |
| `config/settings.json` | User settings, created from Forge's template. Forge's defaults are in `crates/forge-native/assets/forge-defaults.json`. |
| `config/keymap.json` | Key bindings |
| `config/palettes/*.json` | Colour palettes, one theme each. Built-in ones that were never edited are updated with Forge (`theme.rs`, `PREVIOUS_BUILTINS`). |
| `config/agents.json` | The ACP agents threads talk to, MCP servers and agent permissions |
| `config/AGENTS.md` | The user's instructions for agents, sent with the first message of every thread |
| `config/extensions.json` | Extension settings |
| `config/dotnet.json` | Solution Explorer and NuGet options (`configuration`, `properties`, `itemTypes`, `customCommands`, `nestFiles`, `nuget.includePrerelease`…) |
| `agent-sessions/` | Saved agent conversations, per project |
| `themes/*.json` | Full Zed theme files, for those who prefer them to palettes |
| `extensions/` | Installed extensions |

A project's own settings, tasks and debug scenarios live in its `.forge/` folder (patch `0006`), together with its instructions for agents (`.forge/AGENTS.md`), customized file templates (`.forge/templates/`) and the worktrees of threads that work in one (`.forge/worktrees/`, which git ignores).

## Repository layout

```text
crates/
  forge-native/          the app: Zed bootstrap, menus, title and status bars, settings tab, themes
  forge-ui/              pieces shared by Forge crates: panel drag & drop, pickers, settings registry
  forge-extension-host/  QuickJS runtime, UI trees, GPUI rendering, Extensions panel, extension API
  forge-agents/          agent threads (tab, anchored, follow, worktrees) + Zed-project file adapter
  forge-run/             run targets and the run / debug / hot reload controls
  forge-tests/           Tests panel: discovery, runners and debugging for every test kind
  forge-languages/       C# (OmniSharp, netcoredbg), MSBuild, .sln and .http languages; launch profiles
  forge-dotnet/          Solution Explorer, NuGet manager, project file editing, user secrets, EF
  dotnet-model/          .sln/.slnx, MSBuild evaluation and edits, NuGet V3 (no UI)
  forge-git/             Git: Zed's git UI, history graph, merge editor, git init, background fetch
  forge-github/          pull requests and review comments through `gh`
  forge-http/            .http files: requests, environments, responses
  forge-output/          Output panel and language server status
  forge-update/          self-update from GitHub releases
  acp-client, jsonrpc,   ACP runtime used by forge-agents (tokio, editor-agnostic)
  ide-api                contracts between the ACP runtime and its host
packages/forge-api/      @forge/api: reconciler, components, runtime, the forge-ext tool
extensions/              workspace-notes and webview-demo (examples), db-explorer (ships with Forge)
patches/zed/             Forge's changes to Zed, applied by scripts/apply-zed-patches.sh
scripts/                 patches, packaging (bundle-macos.sh), the installer (install.sh), icon
tools/                   the mock ACP agent used by tests
.github/workflows/       CI and releases
vendor/zed/              Zed, as a git submodule pinned to a release tag
```

## Tests

```bash
cargo test --workspace                     # every Forge crate (Zed's crates are not members)
(cd packages/forge-api && npm test)        # the extension runtime, in Node
```

Many tests drive real tools: git, python3 (the mock agent), go and cargo. Tests that need more (the .NET SDK, pytest, Jest or Vitest, a GitHub clone, a slow HTTP server) are `#[ignore]`d, and their comment names the environment variable that points them at a real project, for example:

```bash
FORGE_TESTS_PYTHON_DIR=/path/to/project cargo test -p forge-tests -- --ignored runs_a_real_project
```

`extensions/db-explorer/sidecar` is a separate Cargo project (the workspace excludes `extensions/`), with its own tests. The tree-sitter grammars Forge carries (`crates/forge-languages/grammars/*`) are regenerated with the command at the top of each `grammar.js`.

## Packaging and releases

```bash
scripts/bundle-macos.sh        # → dist/Forge.app and dist/Forge-<version>-<arch>.zip
open dist/Forge.app
```

- **Shaders:** by default the release keeps `runtime-shaders`, so Metal shaders compile at launch. With full Xcode installed, `FORGE_PRECOMPILED_SHADERS=1 scripts/bundle-macos.sh` precompiles them.
- **Signing:** the bundle is signed ad-hoc, which is enough to run it on the machine that built it. For distribution, set `FORGE_SIGN_IDENTITY="Developer ID Application: …"` (hardened runtime, `scripts/Forge.entitlements`). With `FORGE_NOTARY_PROFILE=<profile>` as well (created once with `xcrun notarytool store-credentials`), the script notarizes the app and staples the ticket.
- **CI:** `.github/workflows/ci.yml` runs every test on pushes and pull requests.
- **Releases:** pushing a tag `v<version>` runs `.github/workflows/release.yml`, which takes the version from the tag. It builds the app for Apple silicon and Intel with precompiled shaders, signs and notarizes it when the `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_TEAM_ID` and `APPLE_APP_PASSWORD` secrets are set, and publishes the zips as a GitHub release. Both architectures build on Apple silicon runners (Intel cross-compiled, `FORGE_TARGET=x86_64-apple-darwin scripts/bundle-macos.sh` does the same locally), without debug info. A tag can only reuse build caches saved on `main`, so pushes to `main` that change `Cargo.lock`, Zed or its patches run the same build without publishing, to keep that cache warm (Actions › Release › *Run workflow* does it by hand). With a warm cache a release compiles little more than Forge's own crates.
- **Updates:** Forge updates itself from the GitHub releases of `fernandoescolar/forge` (forks set `FORGE_UPDATE_REPOSITORY=owner/repo` when building). Release builds look for a newer release at startup and every six hours; any build looks on Forge › *Check for Updates…*. A newer `Forge-<version>-<arch>.zip` is downloaded, checked (a validly signed app with Forge's bundle id) and swapped in for the running app, and Forge offers to restart (`crates/forge-update`). GitHub's "latest release" is what counts, so mark a release as a pre-release only if you don't want installed copies to move to it.
- **Cutting a release:** tag the commit and push the tag: `git tag v0.0.2 && git push origin v0.0.2`. The tag is the version (`v1.2.3`, or `v1.2.3-beta.1` for a beta): the release workflow writes it into `Cargo.toml` before building, so the app, its zip and the updater all carry it, and there is nothing to edit first. It builds both architectures and publishes the release; installed copies update within hours. A tag that isn't a version fails the workflow. The app's own version fields get the numeric part only (`0.0.1` for `0.0.1-beta`).
- **Launching:** from Finder or the Dock, Forge opens an empty window. Folders and files dropped on its icon, or opened with *Open With → Forge*, open as projects; `forge [paths…]` from a terminal opens those paths, or the current directory.
- **Icon:** `scripts/make-icon.py` renders it, with no dependencies.

## Zed

Forge uses Zed's crates by path from `vendor/zed`, with a few small patches in `patches/zed/`. [ZED_INTEGRATION.md](ZED_INTEGRATION.md) explains the patches and how to move to a new Zed release:

```bash
scripts/vendor-zed.sh v1.23.0      # fetch and check out the tag, re-apply the patches
# refresh [patch]/[profile] in Cargo.toml and Cargo.lock from vendor/zed, then build and test
git add vendor/zed Cargo.toml Cargo.lock && git commit -m "Update Zed to v1.23.0"
```

## Conventions

- **Changes to Zed are hooks, not features.** Prefer Zed's public API; when that isn't enough, add a small generic hook as a new patch in `patches/zed/` and keep the feature in a Forge crate.
- **Tests come with the change.** Behaviour visible to users gets a test that drives it (GPUI tests with a fake file system, the mock agent, or real tools behind `#[ignore]`).
- **Docs move with the code.** The user guide (`docs/GUIDE.md`, also shown in the app) and the extension guide (`docs/EXTENSIONS.md`) describe what Forge does now; `docs/ROADMAP.md` lists what is done and what is left.
