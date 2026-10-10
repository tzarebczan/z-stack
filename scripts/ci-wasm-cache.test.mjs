import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { wasmCacheIdentity, validateWasm } from "./ci-wasm-cache.mjs";

test("WASM reuse is bound to source, both compiler builds and wasm-pack", () => {
  const key = wasmCacheIdentity("source", ["stable compiler", "nightly compiler"], "0.15.0");
  for (const args of [["changed", ["stable compiler", "nightly compiler"], "0.15.0"],
    ["source", ["new compiler", "nightly compiler"], "0.15.0"],
    ["source", ["stable compiler", "new nightly"], "0.15.0"],
    ["source", ["stable compiler", "nightly compiler"], "new pack"]])
    assert.notEqual(wasmCacheIdentity(...args), key);
  assert.throws(() => wasmCacheIdentity("source", [""], "0.15.0"), /Missing/);
  assert.notEqual(wasmCacheIdentity("source", ["stable compiler", "nightly compiler"], "0.15.0", {RUSTFLAGS:"changed"}), key);
});
test("cached module bytes, mode and source are verified; bindings are included in its receipt", t => {
  const root = mkdtempSync(join(tmpdir(), "z-stack-wasm-cache-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
  writeFileSync(join(root, "z_wasm_bg.wasm"), bytes);
  writeFileSync(join(root, "integrity.json"), JSON.stringify({ sourceSha256: "source", mode: "single-thread",
    sha256: createHash("sha256").update(bytes).digest("hex") }));
  writeFileSync(join(root, "z_wasm.js"), "binding");
  const before = validateWasm(root, "source", "single-thread");
  assert.throws(() => validateWasm(root, "stale", "single-thread"), /fingerprint/);
  assert.throws(() => validateWasm(root, "source", "multi-thread"));
  writeFileSync(join(root, "z_wasm.js"), "altered binding");
  assert.notDeepEqual(validateWasm(root, "source", "single-thread"), before);
  const corrupted = readFileSync(join(root, "z_wasm_bg.wasm")); corrupted[0] = 1;
  writeFileSync(join(root, "z_wasm_bg.wasm"), corrupted);
  assert.throws(() => validateWasm(root, "source", "single-thread"), /integrity failed/);
});
