#!/usr/bin/env node
// A client's shell hook, as Forge's `--agent-shell-hook` answers agy: refuses `cat`, says
// nothing about anything else.
import { readFileSync } from "node:fs";

const call = JSON.parse(readFileSync(0, "utf8"));
const line = call.toolCall?.args?.CommandLine ?? "";
if (/^cat\b/.test(line)) console.log(JSON.stringify({ decision: "deny", reason: "Read files with read_file" }));
