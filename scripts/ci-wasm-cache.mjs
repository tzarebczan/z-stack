import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { appendFileSync, readFileSync, readdirSync, mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { engineSourceHash } from "./wasm-build-inputs.mjs";

const root = resolve(import.meta.dirname, "..");
const hash = bytes => createHash("sha256").update(bytes).digest("hex");
export function wasmCacheIdentity(source, compilers, wasmPack, flags = {}) {
  assert.ok(source && compilers.length === 2 && compilers.every(Boolean) && wasmPack, "Missing WASM compiler identity");
  return hash(JSON.stringify({ version: 1, source, compilers, wasmPack,
    flags: Object.entries(flags).sort() }));
}
function outputs(directory, prefix = "") {
  return readdirSync(join(directory, prefix), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name)).flatMap(entry => {
    if (entry.name === "README.md" || entry.name === ".gitkeep") return [];
    const path = prefix ? `${prefix}/${entry.name}` : entry.name;
    assert.ok(!entry.isSymbolicLink(), "WASM cache must not contain symbolic links");
    return entry.isDirectory() ? outputs(directory, path) : [[path, hash(readFileSync(join(directory, path)))]];
  });
}
export function validateWasm(directory, source, mode) {
  const integrity = JSON.parse(readFileSync(join(directory, "integrity.json"), "utf8"));
  assert.equal(integrity.sourceSha256, source, "Cached WASM source fingerprint is stale");
  assert.equal(integrity.mode, mode);
  const binary = readFileSync(join(directory, "z_wasm_bg.wasm"));
  assert.equal(hash(binary), integrity.sha256, "Cached WASM integrity failed");
  assert.deepEqual([...binary.subarray(0, 8)], [0, 97, 115, 109, 1, 0, 0, 0], "Cache does not contain a WASM module");
  return outputs(directory);
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const action = process.argv[2];
  const source = engineSourceHash();
  if (action === "key") {
    assert.ok(process.env.Z_STACK_WASM_MT_TOOLCHAIN && process.env.Z_STACK_WASM_PACK_VERSION
      && process.env.GITHUB_OUTPUT, "Missing workflow compiler configuration or output file");
    const compilers = [execFileSync("rustc", ["--version", "--verbose"]).toString(),
      execFileSync("rustc", [`+${process.env.Z_STACK_WASM_MT_TOOLCHAIN}`, "--version", "--verbose"]).toString()];
    const flags = Object.fromEntries(Object.entries(process.env).filter(([name]) =>
      /^(RUST|CARGO_PROFILE_|CARGO_ENCODED_RUSTFLAGS|CARGO_BUILD_RUSTFLAGS|CARGO_TARGET_.+_RUSTFLAGS|WASM_BINDGEN)/.test(name)));
    const key = wasmCacheIdentity(source, compilers, process.env.Z_STACK_WASM_PACK_VERSION, flags);
    appendFileSync(process.env.GITHUB_OUTPUT, `key=${key}\n`);
  } else {
    assert.ok(["record", "verify"].includes(action), "Use key, record or verify");
    const entries = [["generated", "single-thread"], ["generated-mt", "multi-thread"]].map(([name, mode]) =>
      [name, validateWasm(join(root, "packages/sdk/src", name), source, mode)]);
    const receipt = { source, entries };
    const path = join(root, ".cache", "ci-wasm.json");
    if (action === "record") {
      mkdirSync(join(root, ".cache"), { recursive: true });
      writeFileSync(path, JSON.stringify(receipt, null, 2) + "\n");
    } else assert.deepEqual(JSON.parse(readFileSync(path, "utf8")), receipt, "Cached WASM bindings or helper files changed");
    console.log(`Verified real ST and MT WASM, source fingerprint and generated file integrity (${action})`);
  }
}
