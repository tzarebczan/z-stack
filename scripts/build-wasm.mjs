#!/usr/bin/env node
/**
 * Build z-wasm for the TS SDK (`packages/sdk/src/generated`).
 * SIMD128 matches legacy WebZjs ST: `+bulk-memory,+mutable-globals,+simd128`.
 */
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { engineSourceHash } from "./wasm-build-inputs.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(root, "packages", "sdk", "src", "generated");
const sourceSha256 = engineSourceHash();
// no-vectorize-slp: newer LLVM packs pairs of 64-bit field multiplies into
// i64x2.mul, which x64 engines emulate (2x slower proofs on the nightly
// multicore build; the pinned stable toolchain does not do it yet).
const rustflags = [process.env.RUSTFLAGS, "-C target-feature=+bulk-memory,+mutable-globals,+simd128 -C no-vectorize-slp"]
  .filter(Boolean)
  .join(" ");
const r = spawnSync(
  "wasm-pack",
  [
    "build",
    ".",
    "--target",
    "web",
    "--release",
    "--out-dir",
    out,
    "--out-name",
    "z_wasm",
  ],
  {
    cwd: join(root, "crates", "z-wasm"),
    stdio: "inherit",
    shell: true,
    env: { ...process.env, RUSTFLAGS: rustflags },
  },
);
if (r.status !== 0) process.exit(r.status ?? 1);
const wasm = join(out, "z_wasm_bg.wasm");
if (!existsSync(wasm) || !readFileSync(wasm).toString("latin1").includes("simd128")) {
  console.error("SIMD128 missing from", wasm);
  process.exit(1);
}
const sha256 = createHash("sha256").update(readFileSync(wasm)).digest("hex");
assert.equal(engineSourceHash(), sourceSha256, "engine source changed during build; rebuild before packing");
writeFileSync(
  join(out, "integrity.json"),
  JSON.stringify({ file: "z_wasm_bg.wasm", sha256, sourceSha256, toolchain: "1.91", mode: "single-thread" }, null, 2) + "\n",
);
console.log("wasm SIMD128 present:", wasm);
console.log("wasm sha256:", sha256);
