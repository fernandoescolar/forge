# Forge Guide

Forge is the opinionated IDE for real-world .NET, Go, Rust and JavaScript development, with agents built in and built on Zed. It is a native, GPU-rendered editor that already knows how to run, test and debug your projects, and how to work on them with an AI agent. This guide covers everything it does.

- [Install and update](#install-and-update)
- [Getting started](#getting-started)
- [Search and replace](#search-and-replace)
- [Panels and layout](#panels-and-layout)
- [Settings](#settings) and [colours and fonts](#colours-and-fonts)
- [Run and debug](#run-and-debug) and the [terminal](#terminal)
- [.NET](#net)
- [Tests](#tests)
- [Agents](#agents)
- [Git and GitHub](#git-and-github)
- [HTTP requests](#http-requests)
- [Database Explorer](#database-explorer)
- [Containers](#containers)
- [Extensions](#extensions)

## Install and update

Forge runs on macOS 13 or later (Apple silicon and Intel) and on Linux (x86_64 and ARM64, with glibc 2.35 or later: Ubuntu 22.04, Debian 12, Fedora 36 and later), under Wayland or X11. To install it, or update it by hand, run this in a terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.sh | bash
```

It downloads the latest release for your Mac from the [releases page](https://github.com/fernandoescolar/forge/releases), checks its signature, quits Forge if it is running, puts Forge.app in Applications (`~/Applications` if you can't write to Applications) and adds a `forge` command to `~/.local/bin`: `forge .` opens the current folder, `forge file.cs` a file, whether Forge is running or not. `curl … | FORGE_VERSION=0.0.2 bash` installs a given version.

On Windows (preview: x86_64), run this in PowerShell instead:

```powershell
irm https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.ps1 | iex
```

It downloads `Forge-<version>-windows-x86_64.zip`, checks it against the SHA-256 the release lists, unpacks it into `%LOCALAPPDATA%\Programs\Forge`, adds Forge to the Start menu and a `forge` command to your PATH. As on Linux, `forge .` while Forge runs opens the folder in it, and a Forge that is running keeps running until you restart it. Forge keeps its settings in `%LOCALAPPDATA%\Forge`.

On Linux it downloads `Forge-<version>-linux-<arch>.tar.gz`, checks it against the SHA-256 the release lists, unpacks it into `~/.local/opt/forge`, adds Forge to your desktop's applications (with its icon) and adds the `forge` command. A Forge that is running keeps running: restart it to use the new one. `forge .` while Forge runs opens the folder in it rather than starting another Forge. The tarball can also be unpacked anywhere by hand: `forge/bin/forge` runs it.

To install from the zip instead, download `Forge-<version>-macos-<arch>.zip` (`aarch64` for Apple silicon, `x86_64` for Intel), unzip it and move **Forge.app** to Applications. Until Forge is signed with an Apple Developer ID, macOS quarantines what the browser downloads, and a quarantined Forge doesn't start (not even after *Open Anyway* in System Settings › Privacy & Security). Take it out of quarantine once:

```bash
xattr -dr com.apple.quarantine /Applications/Forge.app
```

Release builds keep themselves up to date (on Linux, when installed from the tarball): they look for a new version at startup and every few hours and install it in the background. Then a notification in the corner of every window offers to restart now: *Restart Now* asks about unsaved changes, if there are any, and opens the new version with the projects you had open; *Later* (or closing it) keeps working with the current one, and the update applies the next time Forge starts. Forge › *Check for Updates…* looks right away (or offers again an update that is waiting).

Opening a project also checks that the tools it needs are installed: `git` for a Git repository, `cargo` and `rustc` for a Rust crate, `node`, `npm` and `npx` (and `pnpm`, `yarn` or `bun`, from the lock file) for a `package.json`, `go`, Python, and for .NET `dotnet` with an SDK that can build it (the one `global.json` asks for, or the newest `net<N>.0` the projects target). When one is missing, a notification says which, and *Download* opens where to get it.

Forge uses the tools you already have, found through your shell's `PATH`:

| For | Forge uses |
| --- | --- |
| .NET | the .NET SDK (`dotnet`); `dotnet-ef` for migrations, which Forge offers to install |
| Rust, Go | `cargo`, `go`, and the language servers Zed installs |
| JavaScript and TypeScript | Node.js, and your package manager (npm, pnpm, Yarn or Bun) |
| Python | `python3`, or the project's `.venv`; pytest for tests |
| Agents | the agent's own command, as listed in `agents.json` (Claude Code's runs with `npx`) |
| GitHub | the [`gh`](https://cli.github.com) command, signed in with `gh auth login` |

## Getting started

- **Open a folder:** File › Open (⌘O) asks for a folder (or files) and opens it in a new window; the window you asked from closes if it had nothing open. You can also drop a folder on the Dock icon, or run `forge <path>` in a terminal. File › *Open Recent…* (⌥⌘O) picks one of your recent projects. File › *New Window* opens an empty window on the welcome page, with the docks laid out like the window you were in.
- **Coming back:** when you quit (or close the last window), Forge remembers its windows: their projects, tabs and docks. The next start reopens them as they were, also when you start it with a folder (`forge <path>`), which then opens in its own window or brings forward the one that already shows it. A window you close while others stay open is forgotten.
- **Find anything:** ⌘P opens files, ⌘⇧P runs any command, ⌘F finds in the open file and ⌘⇧F searches the project (see [Search and replace](#search-and-replace)).
- **Run and debug:** pick what to run in the title bar, then Run or Debug (see [Run and debug](#run-and-debug)).
- **The tools a project needs:** when a project uses a tool that isn't installed (the .NET SDK for a `.csproj`, Rust for a `Cargo.toml`, Go, Node.js, pnpm or Yarn by its lock file, Python, make, git), a notification names it and offers to install it. Help › *Check Tools…* lists them all, installed or not, with why the project needs each, and GitHub's `gh` and Docker when the project uses them. Installing runs the command shown (your package manager's, such as `sudo pacman -S go` or `brew install go`, or the tool's own installer when it needs no administrator: rustup, pnpm, .NET's install script) in a terminal, where you see what it does and type your password if it asks; Forge then tells you when the tool is there. A tool with no command for your system links to its download page. The .NET debugger, netcoredbg, needs nothing: Forge downloads it the first time you debug. Tools installed where their installers put them (`~/.dotnet`, `~/.cargo/bin`, `~/go/bin`, `~/.local/bin`…) work before you log in again: Forge adds those folders to the `PATH` it runs everything with, at the end, and `~/.dotnet` first when it holds an SDK, so a .NET install of your own wins over an incomplete one of the system's.
- **Get around:** the status bar shows errors and warnings, what the language servers and agents are doing, the debug session, merge conflicts and your branch's pull request; each one opens what it is about.

## Search and replace

- **In the open file:** ⌘F (Edit › Find) opens the find bar above the editor, with the selection or the word under the cursor as the query. Matches highlight as you type; Enter and ⇧Enter (or ⌘G and ⇧⌘G) go to the next and previous one, ⌥Enter selects them all, and Escape closes the bar. ⌥⌘L limits the search to the selection.
- **In the whole project:** ⌘⇧F (Edit › Find in Project) opens a search tab. Type the query and press Enter: the results show as excerpts of every file that matches, which you can edit in place. ⌘⇧F again goes back to the query; Escape moves between the query and the results. ⌘⇧J shows the *include* and *exclude* filters (paths or globs such as `src/**/*.cs`, separated by commas); the button next to them also searches files your `.gitignore` leaves out.
- **Options:** the buttons at the end of the query, or ⌥⌘C *match case*, ⌥⌘W *whole word* and ⌥⌘X *regular expression*. With a regular expression, the replacement can use the groups it captured (`$1`).
- **Replace:** ⇧⌘H (or Edit › Find and Replace) adds the replacement field. Enter replaces the current match and moves to the next; ⌘Enter replaces them all, in the file or across the project.
- **Up and Down** in the query go through your earlier searches.

## Panels and layout

Panels sit on the left, right and bottom of the window. Click a panel's button to show it, and click it again to hide it.

- **Move a panel:** drag its button, or the ⋯ grip in its header, to another side. You can also right-click the button and pick *Dock Left / Right / Bottom*.
- **Show panels together:** drop a panel's button on another's (or right-click › *Show with …*) and the dock shows both, one above the other (side by side in the bottom dock). Clicking either button shows the group; drag the divider between them to resize (double-click it to share evenly again); *Show Alone* separates them.
- **Find a panel:** View › Panels lists every panel, Forge's and the extensions', by name.
- **Hide panels you don't use:** right-click a button › *Hide from the Sidebar*, or use View › *Show or Hide Panels…* to pick them from a list. For example, hide the Solution Explorer when you work with Go. Pick a panel again to bring it back.
- **Extension panels** live in *slots*. Drag a tab onto another slot to group them, within its slot to reorder, or to a window edge to give it a slot of its own.

Forge remembers the layout: where each panel is, which ones are hidden and open, and their sizes for each project.

![The Solution Explorer and the Tests panel shown together in the left dock](images/panel-groups.jpg)

## Settings

**Forge › Settings…** (⌘,) opens the Settings tab: every setting, in sections (Appearance, Editor, Workspace, Files, Languages, Terminal, Git, Debugging, Agents, .NET, Commits, and one per extension that has settings), with a search box. Changes save as you make them; a dot marks what differs from the default, and the arrow next to it resets it. Lists and maps open the file at the right place.

**Settings for one project.** With a folder open, the switch at the top of the tab goes from *User* (your settings, for every project) to the project's name: then the tab shows only what a project can set (languages and formatting, files, the terminal, commits…) and saves it in the project's `.forge/` folder (`.forge/settings.json`, `.forge/git.json`), where it wins over your settings. Settings the project doesn't set show your values; a dot marks those it sets, and the arrow removes them from the project. Commit the `.forge/` files to share them with whoever works on the project.

The settings are stored in files in `~/Library/Application Support/Forge/config` (on Linux, `~/.local/share/forge/config`), and you can also edit them there:

| File | What it holds |
| --- | --- |
| `settings.json` | Your settings, on top of Forge's defaults (*Open Default Settings* lists them all) |
| `keymap.json` | Your key bindings, over the defaults (*Open Default Key Bindings* lists them); Forge applies them when the file changes |
| `palettes/*.json` | Colour palettes, one theme each |
| `themes/*.json` | Zed theme files |
| `icon_themes/*.json` | Zed icon theme files, with their icons |
| `agents.json` | The agents threads can talk to, and MCP servers |
| `dotnet.json` | Solution Explorer and NuGet options |
| `git.json` | Commit options: signing off |
| `extensions.json` | Extension settings |
| `AGENTS.md` | Your instructions for agents, in every project (see [Agents](#agents)) |

Forge applies a file as soon as you save it. Settings for a single project go in `.forge/settings.json` (and `.forge/git.json`) inside the project, and its tasks and debug scenarios go in `.forge/tasks.json` and `.forge/debug.json`. Forge also reads `.vscode/tasks.json` and `.vscode/launch.json`. The project's `.forge/` folder also holds its instructions for agents (`AGENTS.md`), its customized file templates (`templates/`) and the worktrees of threads that work in one (`worktrees/`, which git ignores).

## Colours and fonts

Every file in `config/palettes/` becomes a theme. Forge ships **Forge Dark** and **Forge Light** (the defaults, which follow the system appearance) and **Dracula**. A palette names about 30 colours:

```jsonc
{
  "name": "Dracula",
  "ui":      { "window": "#21222c", "editor": "#282a36", "accent": "#bd93f9", … },
  "syntax":  { "keyword": "#ff79c6", "string": "#f1fa8c", "type": { "color": "#8be9fd", "font_style": "italic" }, … },
  "terminal":{ "red": "#ff5555", … },
  "overrides": { "editor.wrap_guide": "#ffffff10" }
}
```

When you save a palette, its theme reloads. Syntax scopes inherit from their prefix, so `"string"` also colours `string.special` unless you set that one too. To make your own palette, copy a file, change its `"name"` and select it in `settings.json`:

```jsonc
{
  "theme": "My Palette",
  "buffer_font_family": "Hack Nerd Font Mono",
  "buffer_font_size": 14
}
```

**Zed themes.** Forge also loads Zed theme files: put them in `config/themes/` (a theme from a Zed extension is the `.json` in its `themes/` folder). They reload when saved, like palettes.

**File icons.** Forge's icons are **Forge Dark** and **Forge Light** (based on [Catppuccin Icons](https://github.com/catppuccin/zed-icons)), following the system appearance like the colour themes. Forge also ships **Modern Icons**, **Colored Zed Icons**, **VSCode Great Icons** and **Seti Icons**, and Zed's own set as **Zed (Default)**. Pick one with Forge › *Select Icon Theme…*, in Settings › *File icons*, or with `"icon_theme"` in `settings.json` (a name, or `{ "mode": "system", "dark": …, "light": … }`). To add one, put a Zed icon theme file in `config/icon_themes/`; its icon paths are relative to that folder, so a theme from a Zed extension works if you copy its `icon_themes/*.json` there with its `icons/` folder next to it.

![The Project panel with Forge Dark icons: folders, C#, project files, YAML, Helm templates, GitHub workflows, Docker and Makefile](images/file-explorer.jpg)

**From extensions.** Extensions can bring themes and icon themes too. They show in the selectors while the extension is installed. *Install Themes* in either selector opens the Extensions panel. See [EXTENSIONS.md](EXTENSIONS.md#themes-and-icon-themes).

## Run and debug

Pick what to run in the title bar (.NET apps and tests, Rust, Go, `package.json` scripts and Python programs), then Run or Debug. Scripts run with the package manager the project uses (its `packageManager` field or lockfile: npm, pnpm, Yarn or Bun). Python programs are the top-level files of a project with an `if __name__ == "__main__":` block, packages with a `__main__.py`, and Django's `manage.py runserver`; they run with the project's `.venv` (or `venv`) when it has one. A .NET app with several launch profiles in `Properties/launchSettings.json` gets a target per profile; Run and Debug both apply its environment variables, URLs and arguments; the flame button runs a .NET app with hot reload (see [.NET](#net)). The last test result stays next to it.

File-based .NET apps (a single `.cs` that opens with `#:` directives, run with `dotnet run --file`) are targets too, with a target per profile of their `app.run.json`, and debug like any other app. **Aspire** app hosts, a project or a single-file `apphost.cs` with `#:sdk Aspire.AppHost.Sdk`, come first in the list. Forge also finds them in the `.aspire` folder and wherever the Aspire CLI is set up to look (`appHost.path` in `aspire.config.json`, or `appHostPath` in `.aspire/settings.json`), and takes a single-file app host's profiles from `aspire.config.json` when it has no `apphost.run.json`. While one runs, the title bar shows a button that opens its dashboard already signed in (also Run › Open Aspire Dashboard). Debug runs the app host in a terminal and each of its .NET projects (and file-based apps) under the debugger, as Aspire asks for them: breakpoints work in every service, their output still reaches the dashboard, and Stop ends the app host and all of them. Containers and other resources run as usual.

Debugging uses each language's debugger: CodeLLDB for Rust, Delve for Go, debugpy for Python, the JavaScript debugger for Node, and netcoredbg for .NET. Set breakpoints in the gutter; the debugger opens in its panel when a session starts. The program's output, and an input to evaluate expressions, are in the **Debug Console** panel (View › Panels › Debug Console): put it in any dock, or drop its button on the Debug panel's to show them together. Projects can add their own tasks and debug scenarios in `.forge/tasks.json` and `.forge/debug.json`.

**Makefiles.** Each target of a rule has a run button in the gutter: it runs `make <target>` in a terminal, from the Makefile's folder (special targets like `.PHONY` and patterns like `%.o` have none). The targets also show in the file's outline.

**Helm charts.** A chart's templates (`templates/**/*.yaml`, `*.yml`, `*.tpl` and `NOTES.txt`) open as Helm: the template actions (`{{ include "app.labels" . | nindent 4 }}`, `if`, `range`, `.Values…`) are coloured as Go templates, and the YAML around them as YAML. Other files can be mapped to it with `file_types` in your settings.

![Debugging an ASP.NET Core API: stopped at a breakpoint, with the call stack and the variables, and the Debug Console shown with the Debug panel, evaluating an expression](images/debug.jpg)

## Terminal

Terminal › *New Terminal* opens a terminal in the bottom dock, in the project's folder and with your shell. Each terminal is a tab, and you can split the panel to see two side by side. ⌃↩ (or Terminal › *Ask the Agent About the Output*) sends the selection, or the last lines, to an agent (see [Agents](#agents)).

![The terminal in the bottom dock, running the tests](images/terminal.jpg)

## .NET

The **Solution Explorer** shows `.sln`/`.slnx` solutions as Visual Studio does: solution folders, projects, dependencies and nested files. Creating, renaming, moving, copying and deleting files keeps the project files up to date. Build, test, run, watch, pack and publish from the context menu. Apps with launch profiles (`Properties/launchSettings.json`) get a run target per profile in the title bar.

**Configuration and target framework.** Next to the solution in the title bar, a menu chooses the build configuration (Debug, Release and any other the solution defines, with its platform) and, when a project targets several frameworks, the one to use. Everything that builds follows it: Run, Hot Reload and Debug, the Tests panel and the test buttons in the gutter, and the Solution Explorer's Build, Rebuild, Clean, Run, Watch, Test, Pack and Publish; the Solution Explorer and the NuGet tab also read the projects in that configuration. A project that doesn't target the chosen framework runs its last one, and its tests run for every framework. Forge remembers the choice for each solution. (`configuration` and `platform` in `dotnet.json` are only the defaults for reading projects.)

**The C# language server.** C# uses Roslyn, the language server of VS Code's C# extension and C# Dev Kit: Forge downloads it from nuget.org the first time (or uses `roslyn-language-server` from your PATH) and opens in it the solution the Solution Explorer shows, again whenever you choose another. It reports the errors and analyzer warnings of the whole solution, not only of the open files, and restores projects when it needs to. Roslyn runs on .NET 10 or later; without that runtime, Forge uses OmniSharp instead. `csharpLanguageServer` in the .NET settings (`dotnet.json`) chooses: `auto` (the default), `roslyn` or `omnisharp`, from the next start.

**Build errors in Problems.** When a build, rebuild, restore, pack or publish from the Solution Explorer ends, its errors and warnings show in the Problems panel and in the editor, next to the language server's. That includes what the language server can't see, such as NuGet and MSBuild errors (on the project file) and errors in projects it didn't load. Each build replaces what the previous one reported, and Clean clears it. A problem the language server already shows on the same line isn't listed twice. They are marked *dotnet build*.

**Hot reload.** With a .NET app selected in the title bar, the flame button runs it with `dotnet watch`: code changes apply to the running app as you save, and it restarts when a change can't be applied. Stop ends it.

**User secrets and Entity Framework.** A project's context menu has *Manage User Secrets*, which opens its `secrets.json` (and sets up user secrets the first time). Projects that use Entity Framework Core also have *Add Migration…*, *Update Database*, *Remove Last Migration* and *List Migrations*. They run `dotnet ef` in a terminal, with the app that references the project as the startup project, and offer to install `dotnet-ef` when it is missing.

**Fix all occurrences.** On a C# warning or suggestion that has a fix, the code actions menu (⌘↩ or ⌘.) also offers *Fix all … in this file*, *in the project* and *in the solution*, as Visual Studio does (for example *Fix all “Unnecessary using directive” (CS8019) in the project App*). The language server computes the fix; the files it changes open in a tab to review before you save them, and ⌘Z undoes them. With Roslyn the scopes also include *this member* and *this type*, and actions that group several choices (*Introduce constant*, *Suppress or configure issues*…) list each of them as `Group › Choice`.

**Decompiled sources.** Go to definition on a type from a referenced assembly (the .NET libraries, NuGet packages) opens its source, decompiled when the package doesn't ship it, read-only; navigation goes on from there.

**Project properties.** *Properties…* in a project's context menu opens a tab with its main settings, as Visual Studio's project properties: target frameworks, output type, root namespace and assembly name; nullable reference types, language version, implicit usings, warnings as errors, warning level, suppressed warnings, documentation file and unsafe code; invariant globalization, Native AOT and self-contained publishing; and the package's id, version, authors, description and license. Each shows the value in effect (in the configuration chosen in the title bar) and whether the project file sets it or inherits it (from `Directory.Build.props` or the SDK). Click one to change it: the project file gets it, keeping its formatting. *Reset* removes it from the project file so the inherited value applies again.

**Assembly Explorer.** *Explore Assemblies* in a project's context menu lists the assemblies in its build output (its own first, then the ones it depends on, from NuGet packages and other projects). Unfold one to see its types by namespace; *C#* decompiles a type and *IL* shows its intermediate language, read-only. It uses `ilspycmd`, the command-line ILSpy, and offers to install it (`dotnet tool install -g ilspycmd`) when it's missing. Go to definition into a package opens its real source when the package publishes it (SourceLink or embedded sources), and decompiled code otherwise.

**Code Analysis.** Run › .NET › *Code Analysis* opens a tab with the errors, warnings and suggestions of the solution's C# files (the whole solution, not only the open files), grouped by rule: CS8019, CA1822, IDE0005… with how often each occurs. Unfold a rule to see where; click an occurrence to go to it. Each rule's menu sets its severity for the whole solution (`none`, `silent`, `suggestion`, `warning`, `error` or `default`), written to the `.editorconfig` at the root of the workspace as `dotnet_diagnostic.<rule>.severity`; the language server applies it. The code actions on a problem also offer *Suppress or configure issues › Configure … severity*.

**References and type hierarchy.** Above each C# type and member, "N references" (a code lens) finds them; turn code lenses off with `"code_lens": "off"` in your settings. Go › *Type Hierarchy* lists the base types and interfaces of the type at the cursor (↑), the type itself (●) and the types derived from it (↓), indented by depth; pick one to go to it. It works with any language server that offers type hierarchies.

**Global usings.** On the `using` lines at the top of a C# file, ⌘↩ offers *Move usings to GlobalUsings.cs*: they become `global using` directives in that file at the root of the project (created when it doesn't exist; ones it already has aren't repeated), and leave the file. *…in every file of* the project does it for all of its files at once, as does *Move Usings to GlobalUsings.cs* in a C# project's context menu in the Solution Explorer. The changed files open in a tab to review before you save them. Usings inside a namespace, after a file-scoped `namespace` or behind `#if` stay, since moving them would change what they mean. The file is `globalUsingsFile` in the .NET settings (`dotnet.json`), relative to the project's folder: `_Imports.cs`, `Properties/Usings.cs`…

The **NuGet** tab browses, installs, updates and consolidates packages, including with central package management. In project files, package names complete as you type, and outdated versions are marked; ⌘↩ (or ⌘.) updates one.

![The Solution Explorer with a solution of three projects, their dependencies and NuGet packages; reference counts above each member of the open file](images/solution-explorer.jpg)

## Tests

The **Tests** panel (View › Panels › Tests, or Run › *Run All Tests*) finds the tests in the workspace from their source: .NET test projects, Go modules (`func TestXxx(t *testing.T)`), Cargo packages (`#[test]`, `#[tokio::test]`…, unit and `tests/` targets) packages that use Jest or Vitest (`describe` / `it` / `test` in `*.test.*` and `*.spec.*` files) and Python projects (`test*` functions and methods of `Test*` or `unittest.TestCase` classes in `test_*.py` and `*_test.py` files; parametrized cases show under their test). Run everything, a project, a class, package, module or file, or a single test; failed tests can run again on their own. The bug button next to each one runs it in the debugger instead, stopping at breakpoints (.NET, Go, Rust, Python, Jest and Vitest). Results show in the tree and next to each test in the editor's gutter; a failure shows its message with links to the lines it mentions, and *Fix with agent* sends it to an agent. Tests run with each project's tool (`dotnet test`, `go test`, `cargo test`, `npx jest` / `npx vitest`, `python -m pytest` with the project's virtual environment) and your shell's environment. .NET projects whose `global.json` picks Microsoft.Testing.Platform (`"test": { "runner": "Microsoft.Testing.Platform" }`) run with its options (`--report-trx`, or xUnit v3's `--report-xunit-trx`, and `--coverage` for coverage). The output of a run (the list button) can be selected, searched and copied.

**Data-driven tests (.NET).** A test with data rows (xUnit `[InlineData]`, NUnit `[TestCase]`, MSTest `[DataRow]`) unfolds into one row per case, with its arguments, its result and its duration; rows that come from elsewhere (`[MemberData]`, `[TestCaseSource]`) show up after a run. Once a run has named a case, its Run and Debug buttons run that case alone, and its result replaces only its own; double-click goes to its attribute.

**Code coverage (.NET).** *Coverage* in the Tests panel (or Run › *Run All Tests with Coverage*) runs the .NET tests collecting code coverage, with coverlet when a test project references `coverlet.collector` (the test templates do) and otherwise with Microsoft's collector, which comes with `Microsoft.NET.Test.Sdk`: nothing to install. Then a strip in the gutter of every covered file marks the lines that ran (green), the ones that ran without taking all their branches (yellow) and the ones that never ran (red), in the files you open later too; the panel's header shows the share of lines covered. Click it (or Run › *Clear Coverage*) to take the marks off. The test projects' own files are left out.

**Run settings (.NET).** When the workspace has `.runsettings` files, a menu in the Tests panel's header chooses the one `dotnet test` runs with (`--settings`), or none. Forge remembers it for the workspace.

![The Tests panel with a failed test, its message and Fix with agent, and a test with data rows; results in the editor's gutter](images/tests.jpg)

## Agents

Agents work in **threads**. A thread is one conversation with an agent that speaks the [Agent Client Protocol](https://agentclientprotocol.com), such as Claude Code, Codex, Gemini, GitHub Copilot, Google Antigravity or OpenCode. You list agents in `agents.json`; Settings › Agents › *Manage Agents* adds any of those with one click. Agents that run with `npx` (Claude Code, Codex, Copilot, Antigravity) are installed on first use, with their install scripts allowed, which npm 12 would otherwise skip. Their versions are pinned to the ones each Forge release was tested with; remove the `@<version>` in `agents.json` to follow the latest. OpenCode runs the `opencode` command from [its installer](https://opencode.ai): the `opencode-ai` npm package lags behind it. There are three ways to start one:

- **The thread tab** (⌘⇧A, or the spark in the title bar; ⌘⌥N starts a new one). The conversation fills the editor area. You see your messages, the agent's answers, its tool calls with their terminals, plans and permission requests. Under the input are the session's settings (mode, model and others the agent offers) and its usage. **Changes** lists every file the agent modified, with lines added and removed. Click a file, or *Review*, to see the changes as a diff in a tab, then keep or undo each file or all of them.
- **In the code** (⌃↩ in any file). A question card opens right below the selection. It keeps the latest answer, the changes to review and a reply box.
- **Fix with agent**, from an error's popover, Agents › Fix the Problem at the Cursor, or a failed test. Agents › *Fix the Problems in This File* and *… in the Project* (also *Fix with Agent* on the Problems tab) send every error and warning there, errors first. Merge conflicts and pull request reviews go to agents too (see [Git and GitHub](#git-and-github)).

![A thread that fixed a failing test with Forge's tools (what the user is looking at, file outline, find references, check your changes, run tests), the files it changed, and their diff ready to review](images/agents.jpg)

**Reading a thread.** Each turn starts with your message, set apart so it is easy to find when you scroll back, followed by the agent's part: a header with how long it worked (or a timer while it works), its answer, its thoughts folded to their first line, and one compact list of what it did. Reads, searches and finished commands take one line each; click one to see its output or diff. The agent's plan stays in its turn, updated as it goes, so earlier turns keep the plans they followed. Running commands, failures and anything waiting for you stay open. The files the agent changed are listed once, above the input: each with its added and removed lines, *Review* to see them in the review tab, *Keep* / *Undo* for each file or all of them, and *Undo turn*, which puts the files the last turn changed back as they were before it (pressed again, the turn before; if a later turn changed the same files, Forge asks first). While the agent works, a bar above the input says what it is doing now and for how long, in the warning colour when it waits for you, with *Stop* next to it.

While the agent works, the thread **follows it**: the file it reads or writes opens next to the conversation, and while it runs a command, that command's terminal. If an agent isn't signed in, a sign-in card appears and the login runs in a terminal inside Forge. Permission requests can be answered once, always for that tool, or by the mode you choose for the session. By default (Settings › Agents › *Let the agent decide*), agents that have an auto mode, such as Claude Code and Codex, use it: they run what they judge safe without asking, as in their own apps, and only ask about the rest. Agents read your unsaved edits. Writes they ask Forge to make go through the editor (and its review); agents that write files with tools of their own, such as Claude Code, have them recorded as changes right after, to review and undo the same way. Files outside the project's folders are refused, to Forge's tools as well, unless Settings › Agents lets agents reach them; commands run outside the project ask first. The folders for temporary files (`/tmp` and your `$TMPDIR`; `%TEMP%` on Windows) count as the project: agents make, read, change and run scratch files there under the same permission mode, and what they write there goes straight to disk, without review and without joining the thread's changes. When you stop a turn, whatever it was waiting for you on (a permission, a question, a proposed commit) closes, and so does a permission request nobody answers in 30 minutes.

**Reviewing changes.** Wherever a file the agent changed is open, the editor shows what changed since before the agent, with **Keep** and **Undo** on each change (⌘⌥Y / ⌘⌥Z at the cursor). Kept changes become part of the file; undone ones go back as they were and the file is saved. **Checkpoints:** each message you send is one. *Restore checkpoint* on a message puts every file the agent wrote after it back as it was. The pencil on a message puts it back in the input to edit and send again.

**Checking the changes.** After a turn in which the agent changed files, the thread lists their errors and warnings once the language servers have caught up, says whether they are new, and opens each one at its line. *Ask the agent to fix them* sends them back (queued if the agent is working). Turn it off with `verify_changes` in Settings › Agents.

**The composer.** Write in Markdown if you like: your messages show formatted in the thread (lists, code, links), like the agent's answers. Type `@` to mention files, folders and symbols (`@src/app.rs:42`), or `@problems` (the project's errors and warnings), `@diff` (your uncommitted changes) and `@terminal` (the latest terminal output). A leading `/` lists the agent's commands. Paste an image, or drop images and files from Finder, to attach them. While the agent works, Enter queues your message: it goes when the turn ends, or right away with *Send now*. The active file and selection are attached unless you leave them out.

**The Threads panel** lists the open threads and where each stands, and under *Earlier* the project's saved conversations. Hover a row to close a thread (it stays under Earlier) or delete it for good; saved conversations can be deleted the same way.

**Around the IDE.** Git › *Write Commit Message with the Agent* (or alt-tab in the commit box) fills the Git panel's commit box with a message for the staged changes, in the style of your recent commits. In a terminal, ⌃↩ (or Terminal › *Ask the Agent About the Output*) sends the selection, or the last lines, to the agent to explain and fix. When two threads change the same file, both say so.

**Anywhere in Forge.** The status bar shows what the threads are doing (waiting for you, working, files to review) and jumps to them. When a thread finishes or needs you while Forge is in the background, you get a notification. An agent waiting on you (a question, a review) waits until you answer; `tool_timeout_seconds` in Settings › Agents sets a limit instead (Claude Code takes it when its thread starts).

**Commits and pushes the agent proposes.** Agents don't commit or push behind your back. Forge gives them tools to propose a commit or a push (agents that take HTTP MCP servers, such as Claude Code), and asks them to use them. The proposal shows up in the thread as a card with the message and the files. *Commit* commits exactly those files; anything else you had staged stays staged. *Edit in Git panel* stages only them and puts the message in the Git panel, where you change it and commit as usual. *Decline* says no. The agent waits for your answer and hears the result: the new commit's SHA and message, or that you declined. Forge never answers a `git commit` or `git push` command for you, whatever the permission mode (except *Super user*). When an agent asks to run `git commit`, the request also offers *Commit from Forge instead*, which turns it into the same card.

A proposed push shows the branch, where it goes and the commits it takes. It goes to the branch's upstream, or creates the branch on `origin` and makes it the upstream; it is never forced. *Push* pushes it the way the Git panel does, and asks for credentials if they are needed. *Decline* says no. A `git push` the agent asks to run offers *Push from Forge instead*.

**Forge's tools for agents.** Besides committing and pushing, agents that take HTTP MCP servers (Claude Code among them) get tools that act through Forge, so you see what they do. Forge asks them to prefer these tools to doing the same in a shell:
- *Run tests* runs tests in the Tests panel: all of them, those of a file or folder, or those whose name matches. Results show in the gutter, and the agent gets each failure with its place and message.
- *Errors and warnings* reads the language servers' problems, your unsaved edits included, instead of building the project: in a file or folder, in the files the thread changed, or everywhere, and only errors or only warnings if asked.
- *Check your changes* is what agents call after editing: once the language servers have looked at the new code, it lists only the problems the changes brought. The ones a file already had before the thread first changed it are only counted, even when edits moved them to other lines.
- *Go to definition* and *Find references* ask the language server, and so do *File outline* (a file's types and functions with their lines, read before the whole file), *Go to implementations*, *Go to type definition*, *Call hierarchy* (who calls a function, or what it calls: what a change to it affects) and *Signature help*. *Rename a symbol* uses its rename, like the editor's. When writes are reviewed, the rename shows as a card with the places and files it touches, and waits for *Rename* or *Decline*. The renamed files join the thread's changes.
- *Run the app* starts a run target as the title bar's Run does, in a terminal you see. The agent reads its output and can stop it.
- *Run a command* runs a shell command (a build, a script, a test run) in a terminal inside the thread, so you watch its output as it comes rather than when it ends. Unless the permission mode or *Commands that never ask* covers it, the card waits for *Run* or *Don't*. The agent gets the exit code and the output; a command that runs past its time limit (10 minutes unless the agent asks for another) is stopped. Agents whose own shell tool streams its output (through ACP's terminal output) show it live too.
- *Read a file*, *Search the project* and *Search git history* replace `cat`/`head`/`tail`, `grep`/`rg` and `git log`/`show`/`blame`/`diff`: lines of a file from a line or its last ones, Forge's project search (text or a regex, with globs and context lines, your unsaved edits included and ignored files left out), and the history (commits of a file, the ones that added or removed some text, a commit, a file as it was, who changed some lines, a diff). They only read, so they never ask; like the other tools, they refuse paths outside the project unless agents may reach them. *Run a command* also takes `tail`, `head` or `filter`, so the agent gets that part of the output instead of piping into `tail` or `grep`, while you see all of it.
- *Code actions* lists the language server's quick fixes and refactors on some lines, and *Apply a code action* applies one. *Format a file* uses the project's formatter. Like a rename, these show as a card when writes are reviewed, and their files join the thread's changes. *Type and docs* (hover) and *Find symbols* (workspace symbols) read the language server too.
- *Set a breakpoint* (with an optional condition) and *Start debugging* a run target or tests. The agent waits until the program stops and reads the stack and local variables. Then it can *Continue or step*, *Evaluate an expression* and *Stop debugging*. You follow the session in the Debug panel and the stopped line in the editor.
- *Send an HTTP request* goes through the `.http` support: a request of a `.http` file, or one the agent writes, with your selected environment. The response tab shows it.
- *Ask the user* puts a question in the thread, with options as buttons, a box for another answer, and *Skip*. The agent waits for your answer instead of ending its turn. *Notify the user* shows a notification, for example when a long task finishes.
- *What the user is looking at* tells the agent where you are: the file and line, what you selected, your open files, the problems near your cursor, your terminal's last output and the tests that failed in your last run. So "fix this" or "why does this fail?" need no more explaining.
- *Show a file* opens a file at a line or a range for you. *Show your changes* opens the thread's review of its changes.

These calls appear in the thread with their names ("Run tests", "Find references") and what they were asked. *Send an HTTP request* reaches the network and can carry your environments' secrets, so it shows as a card with what the agent asked, and runs only if you *Allow* it (with *Super user*, it runs without asking). *Run tests*, *Run the app*, *Start debugging* and *Evaluate an expression* run your project's code: they ask the same way under *Ask every time* and *Allow file changes*, like commands.

**Shell commands Forge has a tool for.** Agents are told, before they act, to read, search and change files and read the history with these tools and their own file tools, not with `grep`, `cat`, `sed`, `find`, `git log`, pipes into `tail` or `grep`, or `>` into a file. When one tries anyway, Forge refuses the command and tells the agent which tool to use. Each agent takes this its own way, and Forge sets it up when it starts one:

| Agent | Told how | Refused how |
| --- | --- | --- |
| Claude Code | its system prompt | a hook, with the tool to use |
| GitHub Copilot CLI | an instructions file, and the `forge` server's instructions kept | a hook (a plugin of Forge's), with the tool to use |
| Codex | developer instructions | rules in a Codex folder of Forge's (`CODEX_HOME`), with the tool to use |
| Antigravity | the first message | a hook (a plugin the adapter installs while a session runs), with the tool to use |
| Gemini CLI | the first message | a policy (it doesn't pass the reason on) |
| OpenCode | an instructions file (through its config), and the first message | a hook (a plugin of Forge's), with the tool to use; test runs go to *Run tests* |

OpenCode gets Forge's plugin and instructions through `OPENCODE_CONFIG_CONTENT`, on top of your own config (`~/.config/opencode`), and the `forge` server's tools as plain tools rather than behind its Code Mode `execute`. Codex runs with a folder of Forge's (in Forge's data folder) linked to yours (`~/.codex`, or `CODEX_HOME`): the same sign-in, config, `AGENTS.md` and rules, plus Forge's. Codex outside Forge is unchanged; its session history inside Forge is kept apart. Claude Code's thinking is asked for as summaries, so threads show what it thinks rather than only "Working".

**Extensions' tools.** Extensions can offer agents tools of their own, through the same `forge` server. The Database Explorer does: agents can list your connections, read their schema, and run SQL on them, without credentials of their own. So does Containers: agents see your containers and read their logs, and start, stop or restart them, or bring a Compose project up. A tool that only reads runs when an agent calls it. Any other, such as running a query, shows in the thread with its arguments, and runs only if you *Allow* it (with *Super user*, it runs without asking). Agents see an extension's tools in threads started after it loads.

**Another agent, same thread.** Once a thread has started, the agent's name at its top offers the others: choose one to hand the thread over (when the agent isn't working). The conversation stays; the new agent starts a session of its own and gets, with its first message, what was asked, the previous agent's last answer and plan, and the files changed so far, then carries on. Useful when an agent gets stuck or runs out of its quota.

**Files agents don't get.** Secrets are kept from agents: `.env` and `.env.*` (but `.env.example`), keys and certificates (`*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa`…). List more in `.gitignore`'s syntax (`data/customers/`, `*.sqlite`), and give one back with `!` (`!.env.test`): in the project's `.forge/agentignore`, or in an `.agentignore` in any folder, which works like a `.gitignore` there: its patterns are relative to its folder, cover the files below it, and the deepest file that matches a path decides. Settings › Agents › *Files agents don't get* (`agent_ignore_files` in `agents.json`) changes these names: one with a folder in it is relative to the project's root, a bare name counts in every folder. Forge refuses to read or write them for agents, doesn't offer them as `@` mentions (nor sends them if you type one), leaves them out of `@diff`, and refuses permission requests for tools that name them, whatever the permission mode. Agents are also told which they are: some (Claude Code among them) read files with tools of their own without asking, and then that is what keeps them away.

**Threads in a worktree.** Agents › *New Thread in Worktree* starts a thread that works in a git worktree of its own (`.forge/worktrees/<name>`, on branch `forge/<name>`, from your last commit): it never touches the files you are editing, nor another thread's. A bar at the top of the thread shows its branch. *Apply to project* copies its changes into your files as uncommitted changes, to review in the Git panel. Your own uncommitted changes stay: each file is merged with yours, and only where you both changed the same lines are there conflict markers, which the merge editor resolves. *Remove…* deletes the worktree, with or without its branch, and ends the thread. Agents › *Worktrees…* applies or removes any of them, also after a restart.

**Your instructions.** Forge sends standing instructions with the first message of every new session: your `AGENTS.md`, for every project, and the project's own, from `.forge/AGENTS.md` and `AGENTS.md` at its root (every one that exists, so agents that don't read `AGENTS.md` themselves get it too). `instructions_files` in `agents.json` changes which project files those are, relative to the project's root. Edit them from Agents › *Edit Project Instructions* (the first of those files that exists) and *Edit Your Instructions*. The thread notes when they were sent.

**What agents learn.** When an agent finds out something every future session should know (how to build or test the project, a convention, a trap), it can propose a note. The note shows in the thread, editable, with *Remember* and *Don't*: kept, it goes under *Notes from agents* in the project's instructions file (the first one that exists, else `.forge/AGENTS.md`), so the next sessions start knowing it.

**Setting Forge up with an agent.** Agents › *Set Up Forge with an Agent…* (or *Ask an Agent…* in the Settings tab) starts a thread that asks what you'd like: a bigger font in the terminal, no minimap, a key for something you do often. The agent looks up the settings (with their types, defaults and current values, the same ones the Settings tab shows), the key bindings and Forge's guide, then proposes changes on a card, each one *before → after*: *Apply* writes them (keeping your files' comments) and they take effect at once; *Discard* leaves everything as it was. Agents can't change their own setup: which agents run, their permissions and MCP servers stay yours to change, in Settings › Agents.

**Skills and prompts.** Write them once, in Markdown, and every agent gets them, whether it has skills of its own or not:

- A **skill** is a file in the project's `.forge/skills/` (`deploy.md`, or a folder `deploy/` with a `SKILL.md` and whatever files it mentions). Each one is a tool of Forge's `forge` server: agents see its description, and when a task matches they call it and follow the instructions it returns.
- A **prompt** is a file in `.forge/prompts/` (`review.md`). Type `/review` in a thread (it is offered with the agent's own commands) to send it; `$ARGUMENTS` in it becomes what you type after the command (`/review src/parser.rs`), or that text goes after it. The thread shows what you typed and which prompt it sent.

Both may start with front matter, `name:` and `description:` (otherwise the file's name, and the text's first line):

```markdown
---
description: Add a database migration and update the models
---
Create the migration with `make migration NAME=<what it does>`, then …
```

Skills and prompts in Forge's config folder (`skills/` and `prompts/` next to your `AGENTS.md`) are for every project; a project's own replace them by name.

**Adding an agent.** Any agent that speaks ACP over stdio goes in `agents.json` (or Settings › Agents › *Add*): an `id`, the `command` and its `args`, and optionally `env` and `cwd`. Antigravity speaks it through Forge's adapter: install `agy`, then add

```jsonc
{ "id": "antigravity", "command": "npx", "args": ["-y", "@forge-ide/agy-acp"] }
```

Its permission requests come to Forge like any other agent's: the adapter installs a hook (an agy plugin, while its sessions run) that asks before agy edits files or runs commands. If the hook can't be installed, agy's own checks apply and deny what needs a permission.

**MCP servers** listed in `agents.json` are passed to every session:

```jsonc
"mcp_servers": [
  { "name": "github", "command": "github-mcp-server", "args": ["stdio"], "env": { "GITHUB_TOKEN": "…" } }
]
```

## Git and GitHub

Git › *Changes* is the Git panel (stage, commit, push), *History* the commit graph.

**Tags.** Git › *Create Tag…* tags the current commit (HEAD); to tag another one, right-click it in the History panel (or in the Git panel's commit list) and choose *Create Tag…*. Give it a name and, optionally, a message, which makes it an annotated tag. Tick *Push it to …* to send it to the branch's remote (`origin` when the branch tracks none) right away; Forge asks for credentials the way it does for a push.

**Signing off commits.** Projects under the Developer Certificate of Origin (DCO) want each commit signed off: a `Signed-off-by: Name <email>` line, the committer from your git config (`git commit --signoff`). It is the *Sign off commits* setting, on the Settings tab's Commits page: yours applies to every project, and a project's own (saved in its `.forge/git.json`, to commit with it) wins. The Git panel's *Signoff* toggle (in the commit button's menu) and Git › *Sign Off Commits in This Project* turn it on or off for the project. When a project that hasn't chosen has a repository that asks for sign-offs (a `.github/dco.yml` or `DCO` file, a contributing guide that mentions them, or most of its recent commits signed off), Forge offers to turn them on. The commits agents propose follow the same setting: the card has a *Sign off* checkbox showing who signs, and changing it changes it for the project. Agents never write the line themselves; Forge removes any they write.

**Merge conflicts.** Each conflict in the editor has *Use ours*, *Use theirs*, *Use both* and *Resolve with Agent*, which asks a thread to settle that conflict. While the project has conflicts, the status bar offers to resolve all of them with the agent (also Git › *Resolve Conflicts with the Agent*). The agent edits the files; you stage and commit.

**The merge editor** (Git › Merge Conflicts › *Open Merge Editor…*, on the active file or a conflicted one you pick) shows a file with conflicts in three panes: yours and theirs on top, each with the conflicts resolved to that side and highlighted, and the result below, which is the file itself. *Accept Yours*, *Accept Theirs* and *Accept Both* resolve the conflict at the cursor; the arrows move between conflicts, and the top panes follow. You can also edit the result by hand. *Mark as Resolved* saves it and, when git has the file as conflicted, stages it.

**Pull requests** come from GitHub through the [`gh`](https://cli.github.com) command, signed in with `gh auth login`. When the branch you have checked out has a pull request, the status bar shows its number and its checks (failing, running or passed), and keeps them current. Its menu opens it on GitHub, shows its review comments, asks an agent to review it, or lists the others. Git › *Pull Requests…* lists the open ones, to check one out, open it, review it with an agent or show its comments. Review comments show under the lines they are about, in every editor of the file, and a list jumps to each.

## HTTP requests

In a `.http` (or `.rest`) file, click the run button next to a request, or put the cursor on it and press ⌘↩ (or Run › HTTP Requests › *Send Request*). The response opens beside it. Variables come from `@name = value` lines, the environment chosen in Terminal › *HTTP Environment…* (from the nearest `http-client.env.json`, plus `http-client.env.json.user`), earlier requests named with `# @name login` (`{{login.response.body.$.token}}`, `{{login.response.headers.Location}}`) and `{{$guid}}`, `{{$timestamp}}`, `{{$datetime iso8601}}`, `{{$randomInt 1 10}}`, `{{$processEnv NAME}}`, `{{$dotenv NAME}}`.

The response tab shows the status, time, size and headers, then the body (formatted when it is JSON). The body shows as it arrives, so slow and streaming responses (server-sent events, long downloads) fill in as they go. Forge keeps the first 8 MB of a body; a longer one is written to a file as it arrives, and the tab shows its beginning and where the whole of it is. A binary body (an image, a PDF, a zip) is saved to a file, and Forge offers to open it. Each `.http` file has one response tab, reused by every request you send from it; sending another request, or Terminal › *Cancel HTTP Request*, stops the one still running. Terminal › *HTTP Request History…* lists the requests you sent lately, from any file, to send one again.

## Database Explorer

The **Databases** panel (View › Panels › Databases; an extension that comes with Forge) browses SQL Server, PostgreSQL, MySQL, MariaDB, SQLite, MongoDB and Redis databases. Click **+** to add a connection: pick the database, then the server, user and password (kept in the macOS keychain if you leave *Save password* on), or the SQLite file. A MongoDB connection can also be a connection string (`mongodb+srv://…`, as Atlas gives it), and a Redis one a URL (`redis://…`, `rediss://…`) or **Sentinel** (the sentinels, the master's name and their password): Forge connects to the master they name, and finds it again after a failover. Connection strings and passwords stay in the keychain. *Test* tries it first.

- **Browse.** Expand a connection to see its databases, schemas, tables and views. Right-click for more: connect and disconnect, refresh, edit, duplicate or delete the connection, copy a name or a `SELECT`.
- **Rows.** Double-click a table or view to open its rows in a tab, a page at a time. Click a column's header to sort by it, type a condition in *WHERE* and press Enter to filter, and move between pages at the bottom.
- **Edit.** In a table with a primary key, double-click a cell to change it (Enter keeps it, Esc cancels), **+** adds a row and **−** (or Delete) deletes the selected ones. Changes are coloured until you *Save*, which applies them all in one transaction, or *Discard*. Right-click a cell to set it to NULL or copy values. Tables without a primary key are read-only.
- **MongoDB.** Expand a connection to see its databases and their collections; double-click one to open its documents: the first page shows right away, and the menu next to the arrows sets how many per page (10 to 1000; it starts at the *Page size* setting). *Find* takes a filter, a sort and the fields to show, as JSON (`{ "status": "active" }`, `{ "createdAt": -1 }`), and goes a page at a time; *Aggregate* runs a pipeline (`[{ "$match": … }, { "$group": … }]`, ⌘Enter). Besides JSON you can write `ObjectId("…")`, `ISODate("2024-05-31")`, `NumberLong("…")`, `NumberInt(…)`, `NumberDecimal("…")` and `UUID("…")`. Filters, pipelines and documents are highlighted as JSON. The table shows each document's top-level fields; select one to see it whole, as text in that same form, edit it and *Save* (it replaces the document; its `_id` can't change). *Insert Document* adds one; the trash button deletes the selected one.
- **Redis.** Expand a connection to see its databases (`db0`…, those with keys) and, in each, its keys as a tree split at `:` (`user:1:profile` under *user* › *1*), found with `SCAN` a batch at a time (*Load more keys…*), never `KEYS`. With a database (or anything in it) selected, the filter box at the top shows only the keys matching a pattern (`user:*`, `*:session`). Double-click a key to open it: a string as text (highlighted when it is JSON); a hash, list, set, sorted set or stream as a table, a page at a time, where you edit cells, mark rows to delete and *Save* them all, and add entries below. The key's tab also sets or removes its TTL, renames it and deletes it. Right-click a database for *New Key…* and the *Console*, which runs commands as `redis-cli` does (`HGETALL user:1`, `INFO memory`) on the database you pick; right-click a namespace to delete every key under it. Values that aren't text show their bytes as `\xNN`, and are saved from that form.
- **Queries.** *New Query* (the file icon, or right-click › New Query) opens a query tab on the selected connection and database. Write SQL and press ⌘Enter or *Run*: each result appears in its own tab, with what the other statements did under *Messages*. *Cancel* stops a long query. *Database Explorer: Open the Active File (or Selection) in a Query* in the command palette starts one from a `.sql` file.

![The Databases panel with a SQLite and a Redis connection, and a query tab with its results](images/database-explorer.jpg)

Its settings (page size, query row limit, whether to confirm before saving) are in Forge › Settings › Database Explorer.

## Containers

The **Containers** panel (View › Panels › Containers; an extension that comes with Forge) shows your Docker containers, grouped by Compose project, and your images. It uses the `docker` command, so it works with whatever runs the engine: Docker Desktop, OrbStack, Colima, or Podman (set *Docker command* to `podman`). It follows Docker's events, so containers you start or stop anywhere else show up at once.

- **Containers.** Each Compose project shows how many of its containers are running; under it, its services with their status (and health) and published ports (`8080→80`). Containers that Compose didn't start are under *Other containers*. Select one for the buttons at the top: start or stop, restart, follow its logs, open a shell.
- **Logs and shells** open in Forge terminals: double-click a container (or *Follow Logs*) for its last lines and then the new ones as they come, and *Open Shell* runs a shell inside it (`sh`, or the *Shell* setting).
- **More.** Right-click a container to pause or resume it, inspect it (its `docker inspect`, in a tab), copy its id or remove it. Right-click a project for *Up*, *Start*, *Stop*, *Restart*, *Open Compose File* and *Down* (which removes its containers and networks, not its volumes).
- **Images.** The *Images* view lists them with their size and age, the ones in use highlighted; right-click to inspect or remove one.
- **Compose Up.** *Containers: Compose Up* in the command palette runs `docker compose up -d` for the active compose file, or for the project's.

![The Containers panel with a Compose project of three services, its compose file, and a container's logs in a terminal](images/containers.jpg)

Agents get four tools: `containers` and `logs` (with a `grep`) read at once; `control` (start, stop or restart a container or a project) and `compose_up` ask you first. Its settings (the Docker command, the shell, how many log lines to show, whether to confirm before removing) are in Forge › Settings › Containers.

## Extensions

Extensions add panels, tabs, commands, themes and icon themes, and can work with the editor, run programs and keep their own data. They are written in TypeScript with React, and Forge renders them natively. Open the **Extensions** panel to see the installed ones.

Extensions are shared as packages: a `.forgeext` file holds an extension with the programs it needs. To install one, use **Extensions › Install from Package…** (or *Install* in the Extensions panel) and pick the file; Forge checks it, unpacks it into `~/Library/Application Support/Forge/extensions` (on Linux, `~/.local/share/forge/extensions`) and starts it, replacing an older copy. *Install from Folder…* does the same with an extension's folder.

The **Extensions** panel (Extensions › Extensions Panel) shows a card for each extension: its version and description, where it comes from (*Included with Forge*, *Installed* or *Development*), whether it is running or failed (with the error), the panels it adds (click one to show it), its commands and its themes. The ⋯ button has *Reload*, *Settings* (when it has some), *Export as Package…* (a `.forgeext` file to give to someone else), *Reveal in Finder* and, for ones you installed, *Uninstall…*; none needs a restart. The search box filters the list.

What extensions add is also in the menus: their panels under View › Panels, their commands under Extensions › *the extension's name* (and in the command palette). Extensions run with your rights (they can read files and run programs), so install only ones you trust. [EXTENSIONS.md](EXTENSIONS.md) explains how to write one.

## About Forge

Forge is free software under the GPL-3.0-or-later. It is built with open-source libraries from the [Zed](https://zed.dev) editor: GPUI and the editor, terminal and language components.

