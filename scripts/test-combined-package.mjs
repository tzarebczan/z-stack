#!/usr/bin/env node
import { reuseConsumerArchives } from "./consumer-archives.mjs";
// Build both real framework consumers from archives with Base explicitly selected.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createExample } from "./create-example.mjs";
const root = resolve(import.meta.dirname, ".."),
  scratch = mkdtempSync(join(tmpdir(), "z-stack-combined-package-"));
function run(cmd, args, cwd = root) {
  const result = spawnSync(cmd, args, {
    cwd,
    stdio: "inherit",
    shell: process.platform === "win32",
    env: { ...process.env, NEXT_TELEMETRY_DISABLED: "1" },
  });
  assert.equal(result.status, 0, cmd + " failed");
}
try {
  const archives = join(scratch, "archives");
  if (!reuseConsumerArchives(root, archives, ["core", "passkey", "sdk", "base"])) {
    run(process.execPath, ["scripts/pack-sdk.mjs", archives]);
    run(process.execPath, ["scripts/pack-sdk.mjs", archives, "--base-only"]);
  }
  for (const template of ["browser-wallet", "next-wallet"]) {
    const app = join(scratch, template);
    createExample(template, app, archives, { withBase: true });
    run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund"], app);
    const metadata = JSON.parse(readFileSync(join(app, "SDK-ARCHIVES.json")));
    assert.deepEqual(Object.keys(metadata).sort(), [
      "@z-stack/base",
      "@z-stack/core",
      "@z-stack/passkey",
      "@z-stack/sdk",
    ]);
    for (const name of ["sdk", "base"])
      assert.ok(
        !existsSync(join(app, "node_modules/@z-stack", name, "src")),
        "Workspace sources must not leak",
      );
    run("npm", ["run", "build"], app);
    console.log(`${template}: optional Base installed and production build passed`);
  }
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
