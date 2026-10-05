# Zed integration

Forge uses the [Zed](https://zed.dev) editor's crates as libraries: GPUI, the workspace, editor, project, terminal, language and git stacks. This page explains how Zed is pinned, the few patches Forge carries and why, and how to move to a new Zed release. For the rest of Forge's structure see [ARCHITECTURE.md](ARCHITECTURE.md).

## Pinning

`vendor/zed` is a git submodule (shallow) pinned to a Zed release tag. The pinned commit is whatever the superproject records: `git submodule status` shows it. Forge's crates depend on Zed crates by path (`../../vendor/zed/crates/<name>`).

```bash
git submodule update --init --depth 1   # fetch the pinned Zed after cloning Forge
scripts/apply-zed-patches.sh            # then apply Forge's patches to it
```

## Patches

Forge prefers working through Zed's public APIs. When that isn't possible, the change goes into `vendor/zed` and is saved as a patch in `patches/zed/NNNN-<crate>-<what>.patch`, so it survives fresh clones and Zed upgrades:

```bash
# after editing files under vendor/zed
git -C vendor/zed diff -- crates/<crate> > patches/zed/NNNN-<crate>-<what>.patch
```

When an earlier patch already changed the same file, that diff would include its hunks too. Diff a copy of the file from before your edit against the edited one instead, so the new patch applies on top of the earlier ones (`scripts/apply-zed-patches.sh` applies them in order).

Keep each patch small and generic (a hook, not Forge logic), one concern per file:

| Patch | Why |
| --- | --- |
| `0002-editor-run-indicator-status.patch` | `editor::RunIndicatorStatus`: a global hook giving the gutter's run buttons a status (passed, failed, running) when the editor has none of its own; `forge-tests` reports the last test results. Makes `editor::RunnableTaskStatus` public. |
| `0003-workspace-edge-items.patch` | `Workspace::set_edge_item`: views drawn along the left and right edges of the workspace, outside the docks (Forge's panel button strips). |
| `0005-editor-diagnostic-popover-action.patch` | `editor::DiagnosticPopoverAction`: an extra button in the diagnostic popover, under Copy ("Ask the agent to fix this"). |
| `0006-project-config-folder.patch` | Project settings, tasks and debug scenarios live in `.forge/` instead of `.zed/` (`paths::local_settings_folder_name` and the paths built from it, plus the two places in the debugger and the trust prompts that spell the folder out). |
| `0007-workspace-dock-panel-groups.patch` | `Dock::set_panel_groups`: panels shown together in one dock (activating one shows its group, split along the dock with draggable dividers, `Dock::set_panel_weights` for their shares; all of them get `set_active`). Forge's `PanelGroups` decides the groups. |
| `0008-theme-selector-alphabetical.patch` | The theme selector lists themes alphabetically instead of dark ones first. |
| `0009-editor-run-indicator-click.patch` | `RunIndicatorClick`: the application can handle a click on a run indicator itself (forge-http sends the `.http` request on that line instead of running a task). |
| `0010-git-ui-external-conflict-agent.patch` | `ExternalConflictAgent`: with it set, the conflict buttons' *Resolve with Agent* and the status bar's merge conflict indicator show although Zed's AI is disabled; Forge's threads answer their actions. |

Things Forge does without patches, so they don't come back as patches:

- **Restoring the last window:** `"on_last_window_closed": "quit_app"` in Forge's defaults; Zed then keeps the last window in the session.
- **Children that wait for their own children** (`dotnet test`, `dotnet restore`): `forge_ui::process::spawn_unblocked` starts them from a thread with an empty signal mask. GPUI's executor threads block SIGCHLD, and neither std nor `util::command` reset the mask.

Without the patches applied, the build fails on the missing APIs (e.g. `editor::RunIndicatorStatus`).

## Upgrading Zed

1. Run `scripts/vendor-zed.sh <new tag>` (it fetches and checks out the tag inside the submodule, then re-applies `patches/zed`). If a patch no longer applies, the script says how to rebase it.
2. Refresh the integration workspace from `vendor/zed/Cargo.toml`: the `[patch.*]` sections and the `[profile.dev*]` sections. Start from Zed's `Cargo.lock` (`cp vendor/zed/Cargo.lock .`) and let cargo add Forge's own dependencies.
3. Run `cargo build -p forge-native` and fix API drift. It concentrates in:
   - `crates/forge-native/src/main.rs`: the bootstrap, a slim copy of `vendor/zed/crates/zed/src/main.rs`;
   - `crates/forge-agents/src/{threads,thread,terminals,project_fs}.rs`: items, `ui` components, terminal and project APIs;
   - `crates/forge-extension-host/src/{panel,api}.rs`: `ui` components; editor selections, highlights and buffer events;
   - `crates/forge-http` and `crates/forge-github`: editor blocks and the HTTP client;
   - `crates/forge-run` and `crates/forge-tests`: tasks, the DAP store and debug locators.
4. Run the tests, then commit the submodule pointer together with `Cargo.toml` and `Cargo.lock`.


## Notes

- **Shaders:** builds use `gpui_platform/runtime_shaders` by default, so Xcode isn't needed. `FORGE_PRECOMPILED_SHADERS=1 scripts/bundle-macos.sh` turns it off for releases (the release workflow does, on runners with Xcode).
- **Run indicators:** two hooks share the gutter's run buttons: `RunIndicatorStatus` (forge-tests sets it) and `RunIndicatorClick` (forge-http sets it, passing clicks on other files' buttons to whoever set it before).
- **`--printenv`:** Zed captures the login-shell environment by re-running its own executable with `--printenv`. Forge's `main` must keep answering that flag.
- **Separate state:** Forge calls `paths::set_custom_data_dir` with `~/Library/Application Support/Forge`, so it never shares settings or databases with an installed Zed.
- **License:** Zed's crates are GPL-3.0, so Forge is GPL-3.0-or-later.
