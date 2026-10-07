#!/usr/bin/env node
/**
 * Thin wrapper: Sapling params are downloaded by `z-engine` (`z-wallet params`).
 * Orchard/Ironwood do not use these files.
 */
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const r = spawnSync(
  "cargo",
  ["run", "-p", "z-engine", "--features", "native,cli", "--", "params"],
  { cwd: root, stdio: "inherit", shell: true },
);
process.exit(r.status ?? 1);
