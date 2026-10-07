#!/usr/bin/env node
// prepack gate: fail before creating an archive with stale/missing artifacts.
import { spawnSync } from "node:child_process";
import { basePackageInputs } from "./base-package-inputs.mjs";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertReleaseVersions } from "./release-versions.mjs";
import { engineSourceHash } from "./wasm-build-inputs.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
assertReleaseVersions(root);
const name = process.argv[2];
assert.ok(["core", "passkey", "sdk", "base"].includes(name), "expected core, passkey, sdk or base");
const dir = join(root, "packages", name);
const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
for (const entry of Object.values(pkg.publishConfig.exports)) {
  for (const path of Object.values(entry)) assert.ok(existsSync(join(dir, path)), `missing ${path}; run pnpm build:packages`);
}
for (const file of ["README.md", "LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.txt", "THIRD_PARTY_DEPENDENCIES.json"]) assert.ok(existsSync(join(dir, file)), `missing ${file}`);
for (const [target, source] of [["LICENSE", "LICENSE"], ["NOTICE", "NOTICE"],
  ["THIRD_PARTY_LICENSES.txt", "LICENSES.txt"], ["THIRD_PARTY_DEPENDENCIES.json", "dependencies.json"]]) {
  const expected = name === "base" && target !== "LICENSE" ? join(root, "licenses/base", source) :
    target === "THIRD_PARTY_DEPENDENCIES.json" ? join(root, "licenses/WASM-dependencies.json") : join(root, target);
  assert.equal(readFileSync(join(dir, target), "utf8"), readFileSync(expected, "utf8"), `${target} is stale; rebuild packages`);
}
assert.equal(pkg.license, "Apache-2.0", "distribution license must match the approved project license");
assert.equal(pkg.private, true, "publication remains a separate, intentional release step");
if (name === "base") {
  assert.equal(JSON.parse(readFileSync(join(dir, "dist/build-inputs.json"))).sha256, basePackageInputs(dir), "Base sources/config changed; rebuild before packing");
  const legal = spawnSync(process.execPath, [join(root, "scripts/base-license-inventory.mjs"), "--check"], { stdio: "inherit" });
  assert.equal(legal.status, 0, "Base dependency licenses must match the installed runtime");
}
if (name === "sdk") {
  for (const dep of ["core", "passkey"]) {
    const other = JSON.parse(readFileSync(join(root, "packages", dep, "package.json"), "utf8"));
    assert.equal(other.version, pkg.version, `${dep} version must match SDK`);
    assert.equal(pkg.dependencies[`@z-stack/${dep}`], "workspace:*");
  }
  const runtime = readFileSync(join(dir, "dist", "runtime.js"), "utf8");
  assert.ok(runtime.includes(`SDK_VERSION = "${pkg.version}"`), "SDK_VERSION differs from package version");
  for (const kind of ["generated", "generated-mt"]) {
    const generated = join(dir, "dist", kind);
    const wasm = readFileSync(join(generated, "z_wasm_bg.wasm"));
    assert.ok(wasm.byteLength > 1_000_000, "refusing a fixture/stub WASM artifact");
    const integrity = JSON.parse(readFileSync(join(generated, "integrity.json"), "utf8"));
    assert.equal(createHash("sha256").update(wasm).digest("hex"), integrity.sha256, `${kind} integrity mismatch`);
    assert.equal(integrity.sourceSha256, engineSourceHash(), `${kind} source fingerprint is stale; run pnpm build:sdk`);
  }
  for (const worker of ["setup", "scan", "prove"]) assert.ok(existsSync(join(dir, "dist", `${worker}.worker.js`)));
  assert.ok(readdirSync(join(dir, "dist", "generated-mt", "snippets")).length > 0, "missing Rayon worker helpers");
}
console.log(`${pkg.name}@${pkg.version}: distribution files verified`);
