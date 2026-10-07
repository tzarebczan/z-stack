#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
process.env.Z_STACK_REGTEST = "1";
const r = spawnSync(
  "cargo",
  ["test", "--release", "-p", "z-engine", "--test", "regtest", "--", "--ignored", "--nocapture", "--test-threads=1"],
  { cwd: root, stdio: "inherit", shell: true, env: process.env },
);
process.exit(r.status ?? 1);
