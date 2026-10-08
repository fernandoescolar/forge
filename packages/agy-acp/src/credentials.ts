// Sign-in for agy, run by the client as terminal auth methods (`--login`, `--api-key`):
// a Google account (agy's own interactive login) or a Gemini API key kept in the macOS
// Keychain (a private file elsewhere) and handed to agy as GEMINI_API_KEY.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { agyBinary } from "./agy.js";

const SERVICE = "forge-agy-acp";
const ACCOUNT = "GEMINI_API_KEY";

function keyFile(): string {
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "forge-agy-acp", "api-key");
}

/** The stored API key, if any (the environment's GEMINI_API_KEY wins). */
export function storedApiKey(): string | undefined {
  if (process.platform === "darwin") {
    const found = spawnSync("security", ["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"], { encoding: "utf8" });
    const key = found.status === 0 ? found.stdout.trim() : "";
    return key || undefined;
  }
  try {
    return readFileSync(keyFile(), "utf8").trim() || undefined;
  } catch {
    return undefined;
  }
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
  process.stdout.write("Paste a Gemini API key (from Google AI Studio). It is kept " + (process.platform === "darwin" ? "in the macOS Keychain" : `in ${keyFile()}`) + " and passed to agy as GEMINI_API_KEY.\n");
  const key = await askHidden("API key: ");
  if (!key) {
    process.stderr.write("No key given; nothing changed.\n");
    return 1;
  }
  if (process.platform === "darwin") {
    if (!/^[A-Za-z0-9._-]+$/.test(key)) {
      process.stderr.write("That doesn't look like an API key (letters, digits, '-', '_' and '.').\n");
      return 1;
    }
    // `security -i` reads the command from stdin: the key is never on a command line.
    const saved = spawnSync("security", ["-i"], { input: `add-generic-password -U -s ${SERVICE} -a ${ACCOUNT} -w ${key}\n`, encoding: "utf8" });
    if (saved.status !== 0) {
      process.stderr.write(`Couldn't save it in the Keychain: ${saved.stderr.trim()}\n`);
      return 1;
    }
  } else {
    mkdirSync(join(keyFile(), ".."), { recursive: true, mode: 0o700 });
    writeFileSync(keyFile(), key + "\n", { mode: 0o600 });
  }
  process.stdout.write("Saved. New sessions use it.\n");
  return 0;
}
