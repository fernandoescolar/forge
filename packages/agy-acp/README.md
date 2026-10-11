# @forge-ide/agy-acp

An [Agent Client Protocol](https://agentclientprotocol.com) adapter for Google Antigravity's `agy` CLI, so editors that speak ACP (Forge, Zed, JetBrains…) can use it as an agent.

It runs `agy` in its documented stream-json print mode (`--input-format stream-json --output-format stream-json`), one process per conversation, and translates:

- prompts into agy user messages, and agy's steps into ACP message chunks and tool calls (read, edit, search, execute, fetch);
- file edits into whole-file diffs: agy only says which file it edits, so the adapter keeps each file as agy last read or wrote it and diffs against that;
- the session id: it is agy's conversation id, so `session/load` continues a conversation (`--conversation`);
- the model and the reasoning effort into session options (`--model`, `--effort`).

## Use

```json
{ "id": "antigravity", "command": "npx", "args": ["-y", "@forge-ide/agy-acp"] }
```

`agy` must be installed (or set `AGY_BIN`). `AGY_EXTRA_ARGS` adds arguments to every agy run.

## Signing in

The adapter offers two terminal sign-in methods, which ACP clients run for you:

- **Gemini API key** (`forge-agy-acp --api-key`): paste a key from Google AI Studio. It is kept in the macOS Keychain; on Linux in your keyring through Secret Service (GNOME Keyring, KWallet; it needs `secret-tool`, from the `libsecret-tools` package on Debian and Ubuntu), or, without one, in `~/.config/forge-agy-acp/api-key`, readable only by you. It is given to agy as `GEMINI_API_KEY`. An API key is the safer choice for using agy from other tools.
- **Google account** (`forge-agy-acp --login`): agy's own interactive sign-in.

## Permissions

agy's print mode can't ask for permission: it denies what needs one. So the adapter installs a `PreToolUse` hook (as an agy plugin, `forge-guard-<hash>`, one per install, while its sessions run) that asks the client with `session/request_permission` before agy edits files, runs commands or uses other tools that change things (reads and searches don't ask), and runs agy with its own checks off:

| Mode | What happens |
| --- | --- |
| `default` (Ask) | Asks before edits, commands and other tools that change things. |
| `acceptEdits` (Edit files) | Edits the project's files without asking; asks before commands, and edits elsewhere. |
| `bypassPermissions` (Full access) | Edits and runs commands without asking (`--dangerously-skip-permissions`). |
| `plan` | Plans without changing anything (`--mode plan`). |

If the plugin can't be installed, the asking modes fall back to agy's own checks (`default`: none; `acceptEdits`: `--mode accept-edits`), which deny what needs a permission, and the adapter says so. The hook only acts for agy processes the adapter started: your own `agy` runs are left alone. A client can also give a command to run before each shell command (`_meta.forge.shellHook.command` on `session/new`), which may refuse it.

A new model or effort applies from the next prompt, and so does a mode, except between the two asking modes. `FORGE_AGY_ACP_MODE=acceptEdits` (or `bypassPermissions`, `plan`) makes new sessions start in that mode instead.

Images in prompts are saved to a temporary folder (given to agy with `--add-dir`) and agy looks at them with its `view_file` tool.

## Limits

- agy ignores the stream's `interrupt`, so cancelling sends it SIGINT (it ends the turn and exits; it is killed if it doesn't within 2 s); the next prompt continues the same conversation.
- agy doesn't report its reasoning text (only token counts), nor which MCP servers a session should use: configure those with `agy mcp`.

## Develop

```bash
npm install
npm test
```

The tests run the adapter against `test-fixtures/fake-agy.mjs`, which mimics agy 1.3's stream-json events.
