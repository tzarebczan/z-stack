#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const compose = join(root, "infra", "compose", "docker-compose.regtest.yml");
const r = spawnSync(
  "docker",
  ["compose", "-f", compose, "down"],
  { cwd: root, stdio: "inherit", shell: true },
);
process.exit(r.status ?? 1);
