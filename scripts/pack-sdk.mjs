#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = resolve(root, process.argv[2] ?? "artifacts");
mkdirSync(out, { recursive: true });
for (const name of (process.argv.includes("--base-only") ? ["base"] : ["core", "passkey", "sdk"])) {
  const result = spawnSync("pnpm", ["pack", "--pack-destination", out], {
    cwd: join(root, "packages", name), stdio: "inherit", shell: process.platform === "win32",
  });
  if (result.status !== 0) process.exit(result.status ?? 1);
}
console.log(`SDK preview archives: ${out}`);
