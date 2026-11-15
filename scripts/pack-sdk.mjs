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
const optionalModes = ['base', 'gift-cards', 'solana', 'swaps'].filter(name => process.argv.includes(`--${name}-only`));
assert.ok(optionalModes.length <= 1, 'select one optional package');
const selected = optionalModes[0];
const names = selected === 'swaps' ? ['solana', 'swaps'] : selected ? [selected] : ['core', 'passkey', 'sdk'];
for (const name of names) {
  const pkg = join(root, "packages", name);
  if (name !== "sdk" && name !== "swaps") {
    run("pnpm", ["pack", "--pack-destination", out], pkg);
    continue;
  }
  run(process.execPath, [join(root, "scripts/check-package.mjs"), name], root);
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
    const bundled = name === "sdk" ? ["core", "passkey"] : ["solana"];
    manifest.bundleDependencies = bundled.map(dep => `@z-stack/${dep}`);
    for (const dep of bundled) {
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
console.log(`Distribution archives: ${out}`);
