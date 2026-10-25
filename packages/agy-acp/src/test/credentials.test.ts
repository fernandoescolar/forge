import { test } from "node:test";
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { saveApiKey, storedApiKey } from "../credentials.js";

/**
 * A `secret-tool` that keeps the secret in a file, with the attributes it was given; with
 * FAKE_SECRET_FAIL set it fails like secret-tool without a keyring daemon.
 */
function fakeSecretTool(dir: string): string {
  const bin = join(dir, "bin");
  mkdirSync(bin, { recursive: true });
  const script = join(bin, "secret-tool");
  writeFileSync(
    script,
    `#!/bin/sh
[ -n "$FAKE_SECRET_FAIL" ] && { echo "Cannot autolaunch D-Bus without X11 \\$DISPLAY" >&2; exit 1; }
store="$FAKE_SECRET_STORE"
case "$1" in
  store) shift; echo "$*" > "$store.attributes"; cat > "$store" ;;
  lookup) [ -f "$store" ] && cat "$store" || exit 1 ;;
esac
`,
  );
  chmodSync(script, 0o755);
  return bin;
}

test("on Linux the API key goes to the keyring, else to a private file", () => {
  const dir = mkdtempSync(join(tmpdir(), "agy-acp-credentials-"));
  const saved = { path: process.env.PATH, config: process.env.XDG_CONFIG_HOME, store: process.env.FAKE_SECRET_STORE, fail: process.env.FAKE_SECRET_FAIL };
  const keyFile = join(dir, "config", "forge-agy-acp", "api-key");
  try {
    process.env.XDG_CONFIG_HOME = join(dir, "config");
    process.env.FAKE_SECRET_STORE = join(dir, "keyring");
    delete process.env.FAKE_SECRET_FAIL;
    const withSecretTool = `${fakeSecretTool(dir)}:${saved.path}`;

    // No keyring daemon: the file, readable only by the user.
    process.env.PATH = withSecretTool;
    process.env.FAKE_SECRET_FAIL = "1";
    assert.equal(saveApiKey("key-in-a-file", "linux"), "file");
    assert.equal(readFileSync(keyFile, "utf8"), "key-in-a-file\n");
    assert.equal(statSync(keyFile).mode & 0o777, 0o600);
    assert.equal(storedApiKey("linux"), "key-in-a-file");

    // With a keyring: the key is kept there, and the file from before goes.
    delete process.env.FAKE_SECRET_FAIL;
    assert.equal(saveApiKey("key-in-the-keyring", "linux"), "secret-service");
    assert.equal(readFileSync(join(dir, "keyring"), "utf8"), "key-in-the-keyring", "the key reaches secret-tool on its input");
    assert.equal(readFileSync(join(dir, "keyring.attributes"), "utf8").trim(), "--label=Gemini API key (Forge's agy adapter) service forge-agy-acp account GEMINI_API_KEY");
    assert.ok(!existsSync(keyFile), "no copy left in a file");
    assert.equal(storedApiKey("linux"), "key-in-the-keyring");

    // No secret-tool at all (libsecret's tools not installed): the file.
    process.env.PATH = "/nonexistent";
    assert.equal(saveApiKey("key-without-secret-tool", "linux"), "file");
    assert.equal(storedApiKey("linux"), "key-without-secret-tool");
  } finally {
    process.env.PATH = saved.path;
    for (const [name, value] of [["XDG_CONFIG_HOME", saved.config], ["FAKE_SECRET_STORE", saved.store], ["FAKE_SECRET_FAIL", saved.fail]] as const) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
  }
});
