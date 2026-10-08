#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = resolve(root, process.argv[2] ?? "artifacts");
mkdirSync(out, { recursive: true });
function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, stdio: "inherit", shell: process.platform === "win32" });
  assert.equal(result.status, 0, `${command} failed to pack distribution`);
}
const names = process.argv.includes("--base-only") ? ["base"] : ["core", "passkey", "sdk"];
for (const name of names) {
  const pkg = join(root, "packages", name);
  if (name !== "sdk") {
    run("pnpm", ["pack", "--pack-destination", out], pkg);
    continue;
  }
  run(process.execPath, [join(root, "scripts/check-package.mjs"), "sdk"], root);
  // Stage only declared distribution files. Include the unpublished, dependency-
  // free helpers so a lone SDK archive never looks them up in the public registry.
  const scratch = mkdtempSync(join(tmpdir(), "z-stack-pack-"));
  try {
    const manifest = JSON.parse(readFileSync(join(pkg, "package.json")));
    for (const file of manifest.files) cpSync(join(pkg, file), join(scratch, file), { recursive: true });
    manifest.exports = manifest.publishConfig.exports;
    delete manifest.publishConfig;
    delete manifest.scripts;
    delete manifest.devDependencies;
    manifest.bundleDependencies = ["@z-stack/core", "@z-stack/passkey"];
    for (const dep of ["core", "passkey"]) {
      const dependency = join(scratch, "node_modules", "@z-stack", dep);
      mkdirSync(dependency, { recursive: true });
      // These archives were built and checked immediately above.
      run("tar", ["-xzf", join(out, `z-stack-${dep}-${manifest.version}.tgz`), "--strip-components=1", "-C", dependency], root);
      manifest.dependencies[`@z-stack/${dep}`] = manifest.version;
    }
    writeFileSync(join(scratch, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
    run("npm", ["pack", "--ignore-scripts", "--pack-destination", out], scratch);
  } finally { rmSync(scratch, { recursive: true, force: true }); }
}
console.log(`SDK preview archives: ${out}`);
