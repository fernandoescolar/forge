#!/usr/bin/env node
// @forge-ide/agy-acp: Google Antigravity's agy CLI as an Agent Client Protocol agent.
//   forge-agy-acp            ACP over stdio (what clients launch)
//   forge-agy-acp --login    sign in with a Google account (agy's own login)
//   forge-agy-acp --api-key  keep a Gemini API key for agy

import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";
import { AgyAgent } from "./agent.js";
import { login, setApiKey } from "./credentials.js";
import { VERSION } from "./version.js";

const flag = process.argv[2];
if (flag === "--login") {
  process.exit(await login());
} else if (flag === "--api-key") {
  process.exit(await setApiKey());
} else if (flag === "--version") {
  process.stdout.write(VERSION + "\n");
} else {
  // stdout carries the protocol: anything else goes to stderr.
  console.log = console.error;
  const stream = acp.ndJsonStream(Writable.toWeb(process.stdout), Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>);
  new acp.AgentSideConnection((client) => new AgyAgent(client), stream);
}
