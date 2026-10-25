#!/usr/bin/env node
// agy's PreToolUse hook, run by the adapter's plugin (see `hooks.ts`) as
// `node hook.js <plugin>`: agy's input on stdin, the answer on stdout (see `permissions.ts`).

import { hookAnswer } from "./permissions.js";

let input = "";
process.stdin.setEncoding("utf8");
for await (const chunk of process.stdin) input += chunk;
const answer = await hookAnswer(input, process.argv[2] ?? "");
if (answer) process.stdout.write(answer + "\n");
