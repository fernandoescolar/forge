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

agy's print mode can't ask for permission: it denies what needs one. So permissions are the session's mode, chosen up front:

| Mode | agy | What happens |
| --- | --- | --- |
| `bypassPermissions` (default) | `--dangerously-skip-permissions` | Edits and runs commands without asking. |
| `editsOnly` | `--mode accept-edits` | Reads and edits files; commands are denied, and the adapter says so. |
| `plan` | `--mode plan` | Plans without changing anything. |

A new mode, model or effort applies from the next prompt. `FORGE_AGY_ACP_MODE=editsOnly` (or `plan`) makes new sessions start in that mode instead.

## Limits

- agy ignores the stream's `interrupt`, so cancelling stops the agy process; the next prompt continues the same conversation.
- agy doesn't report its reasoning text (only token counts), nor which MCP servers a session should use: configure those with `agy mcp`.

## Develop

```bash
npm install
npm test
```

The tests run the adapter against `test-fixtures/fake-agy.mjs`, which mimics agy 1.3's stream-json events.
