// Sign-in for agy, run by the client as terminal auth methods (`--login`, `--api-key`):
// a Google account (agy's own interactive login) or a Gemini API key handed to agy as
// GEMINI_API_KEY. The key is kept in the macOS Keychain; on Linux in the Secret Service
// keyring (GNOME Keyring, KWallet) through `secret-tool`, or, without one, in a file only
// the user can read.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { agyBinary } from "./agy.js";

const SERVICE = "forge-agy-acp";
const ACCOUNT = "GEMINI_API_KEY";

function keyFile(): string {
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "forge-agy-acp", "api-key");
}

/** Where a key is kept. */
type Place = "keychain" | "secret-service" | "file";

const placeName = (place: Place) =>
  place === "keychain" ? "the macOS Keychain" : place === "secret-service" ? "your keyring (Secret Service)" : keyFile();

/** The Secret Service item's attributes (`secret-tool lookup service … account …`). */
const SECRET_ATTRIBUTES = ["service", SERVICE, "account", ACCOUNT];

/** The stored API key, if any (the environment's GEMINI_API_KEY wins). */
export function storedApiKey(platform: NodeJS.Platform = process.platform): string | undefined {
  if (platform === "darwin") {
    const found = spawnSync("security", ["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"], { encoding: "utf8" });
    const key = found.status === 0 ? found.stdout.trim() : "";
    return key || undefined;
  }
  const found = spawnSync("secret-tool", ["lookup", ...SECRET_ATTRIBUTES], { encoding: "utf8", timeout: 10_000 });
  if (found.status === 0 && found.stdout.trim()) return found.stdout.trim();
  try {
    return readFileSync(keyFile(), "utf8").trim() || undefined;
  } catch {
    return undefined;
  }
}

/**
 * Keeps `key` for the next sessions: in the Keychain on macOS; on Linux in the keyring when
 * there is one (then a key file left from before goes), else in the key file. Returns where.
 */
export function saveApiKey(key: string, platform: NodeJS.Platform = process.platform): Place {
  if (platform === "darwin") {
    if (!/^[A-Za-z0-9._-]+$/.test(key)) throw new Error("That doesn't look like an API key (letters, digits, '-', '_' and '.').");
    // `security -i` reads the command from stdin: the key is never on a command line.
    const saved = spawnSync("security", ["-i"], { input: `add-generic-password -U -s ${SERVICE} -a ${ACCOUNT} -w ${key}\n`, encoding: "utf8" });
    if (saved.status !== 0) throw new Error(`Couldn't save it in the Keychain: ${saved.stderr.trim()}`);
    return "keychain";
  }
  // `secret-tool store` reads the secret from stdin, too. It fails without a keyring daemon
  // (a server, a minimal session): then the file.
  const stored = spawnSync("secret-tool", ["store", "--label=Gemini API key (Forge's agy adapter)", ...SECRET_ATTRIBUTES], { input: key, encoding: "utf8", timeout: 30_000 });
  if (stored.status === 0) {
    rmSync(keyFile(), { force: true });
    return "secret-service";
  }
  mkdirSync(join(keyFile(), ".."), { recursive: true, mode: 0o700 });
  writeFileSync(keyFile(), key + "\n", { mode: 0o600 });
  return "file";
}

/** The environment agy runs with: ours, plus the stored key when none is set. */
export function agyEnv(): NodeJS.ProcessEnv {
  const env = { ...process.env };
  if (!env.GEMINI_API_KEY && !env.GOOGLE_API_KEY) {
    const key = storedApiKey();
    if (key) env.GEMINI_API_KEY = key;
  }
  return env;
}

/** `--login`: agy's own sign-in with a Google account, in this terminal. */
export async function login(): Promise<number> {
  process.stdout.write("Sign in to Antigravity with your Google account in agy below.\nWhen it says you're signed in, quit agy (type /exit, or press Ctrl+C twice) to come back.\n\n");
  return new Promise((resolve) => {
    const child = spawn(agyBinary(), [], { stdio: "inherit" });
    child.on("error", (e) => {
      process.stderr.write(`Can't run agy: ${e.message}\n`);
      resolve(1);
    });
    child.on("exit", (code) => resolve(code ?? 0));
  });
}

function askHidden(question: string): Promise<string> {
  return new Promise((resolve) => {
    const rl = createInterface({ input: process.stdin, output: process.stdout, terminal: true });
    // Echo nothing while the key is typed.
    (rl as unknown as { _writeToOutput: (s: string) => void })._writeToOutput = (s: string) => {
      if (s.includes(question)) process.stdout.write(question);
    };
    rl.question(question, (answer) => {
      rl.close();
      process.stdout.write("\n");
      resolve(answer.trim());
    });
  });
}

/** `--api-key`: asks for a Gemini API key and keeps it for the next sessions. */
export async function setApiKey(): Promise<number> {
  const where = process.platform === "darwin" ? "in the macOS Keychain" : "in your keyring (or, without one, in a file only you can read)";
  process.stdout.write(`Paste a Gemini API key (from Google AI Studio). It is kept ${where} and passed to agy as GEMINI_API_KEY.\n`);
  const key = await askHidden("API key: ");
  if (!key) {
    process.stderr.write("No key given; nothing changed.\n");
    return 1;
  }
  try {
    const place = saveApiKey(key);
    process.stdout.write(`Saved in ${placeName(place)}. New sessions use it.\n`);
    return 0;
  } catch (e) {
    process.stderr.write(`${e instanceof Error ? e.message : e}\n`);
    return 1;
  }
}
