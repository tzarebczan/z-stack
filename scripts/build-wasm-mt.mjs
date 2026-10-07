#!/usr/bin/env node
/**
 * Multicore z-wasm: atomics + wasm-bindgen-rayon + SIMD128.
 * Needs nightly rust-src. Output: packages/sdk/src/generated-mt
 */
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";
import { existsSync, mkdirSync, writeFileSync, readdirSync, readFileSync } from "node:fs";
import assert from "node:assert/strict";
import { engineSourceHash } from "./wasm-build-inputs.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(root, "packages", "sdk", "src", "generated-mt");
const sourceSha256 = engineSourceHash();
const toolchain = process.env.Z_STACK_WASM_MT_TOOLCHAIN || "nightly-2026-09-22";

const rustflags = [
  "--cfg",
  "getrandom_backend=\"wasm_js\"",
  "-C",
  "target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128",
  // Nightly's LLVM packs pairs of independent 64-bit field multiplies into
  // i64x2.mul, which x64 engines emulate: proofs and the scan ran about 2x
  // slower per thread than the stable ST build. See docs/PERFORMANCE.md.
  "-C",
  "no-vectorize-slp",
  "-C",
  "link-arg=--shared-memory",
  "-C",
  "link-arg=--max-memory=4294967296",
  "-C",
  "link-arg=--import-memory",
  "-C",
  "link-arg=--export=__wasm_init_tls",
  "-C",
  "link-arg=--export=__tls_size",
  "-C",
  "link-arg=--export=__tls_align",
  "-C",
  "link-arg=--export=__tls_base",
].join(" ");

function run(cmd, args, extraEnv = {}) {
  const r = spawnSync(cmd, args, {
    cwd: root,
    stdio: "inherit",
    shell: true,
    env: { ...process.env, ...extraEnv },
  });
  if (r.status !== 0) {
    process.exit(r.status ?? 1);
  }
}

const ver = spawnSync("rustup", ["run", toolchain, "rustc", "--version"], {
  encoding: "utf8",
  shell: true,
});
if (ver.status !== 0) {
  console.error(
    `multicore wasm needs ${toolchain}.\n` +
      `  rustup toolchain install ${toolchain} --component rust-src\n` +
      `  rustup component add rust-src --toolchain ${toolchain}\n` +
      `  rustup target add wasm32-unknown-unknown --toolchain ${toolchain}`,
  );
  process.exit(ver.status ?? 1);
}

mkdirSync(out, { recursive: true });

run(
  "rustup",
  [
    "run",
    toolchain,
    "cargo",
    "build",
    "-p",
    "z-wasm",
    "--lib",
    "--release",
    "--target",
    "wasm32-unknown-unknown",
    "--features",
    "multicore,prove",
    "-Z",
    "build-std=panic_abort,std",
  ],
  {
    RUSTUP_TOOLCHAIN: toolchain,
    RUSTFLAGS: rustflags,
    CARGO_PROFILE_RELEASE_PANIC: "abort",
  },
);

const targetDir = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const wasm = join(targetDir, "wasm32-unknown-unknown", "release", "z_wasm.wasm");
const bindgen = process.env.WASM_BINDGEN || findBindgen();

// wasm-bindgen on PATH, else the copy wasm-pack cached for the ST build
// (same version: both follow Cargo.lock).
function findBindgen() {
  if (spawnSync("wasm-bindgen", ["--version"], { stdio: "ignore" }).status === 0) return "wasm-bindgen";
  const lock = readFileSync(join(root, "Cargo.lock"), "utf8");
  const want = /name = "wasm-bindgen"\r?\nversion = "([^"]+)"/.exec(lock)?.[1];
  if (!want) return "wasm-bindgen";
  const cache = join(homedir(), ".cache", ".wasm-pack");
  for (const dir of existsSync(cache) ? readdirSync(cache) : []) {
    if (!dir.startsWith("wasm-bindgen-")) continue;
    const bin = join(cache, dir, "wasm-bindgen");
    const version = spawnSync(bin, ["--version"], { encoding: "utf8" }).stdout?.trim();
    if (version?.endsWith(want)) return bin;
  }
  return "wasm-bindgen";
}

run(bindgen, [wasm, "--out-dir", out, "--typescript", "--target", "web", "--out-name", "z_wasm"]);
writeFileSync(
  join(out, "index.js"),
  'export { default } from "./z_wasm.js";\nexport * from "./z_wasm.js";\n',
);
writeFileSync(
  join(out, "package.json"),
  JSON.stringify(
    { type: "module", name: "z-wasm-mt", main: "./z_wasm.js", exports: { ".": "./index.js" } },
    null,
    2,
  ),
);
function patchWorkerHelpers(dir) {
  const snippets = join(dir, "snippets");
  let names = [];
  try {
    names = readdirSync(snippets);
  } catch {
    return;
  }
  for (const name of names) {
    const helper = join(snippets, name, "src", "workerHelpers.js");
    try {
      const src = readFileSync(helper, "utf8");
      writeFileSync(helper, src.replaceAll("import('../../..')", "import('../../../z_wasm.js')"));
    } catch {
      /* optional */
    }
  }
}
patchWorkerHelpers(out);
const bg = join(out, "z_wasm_bg.wasm");
if (!existsSync(bg) || !readFileSync(bg).toString("latin1").includes("simd128")) {
  console.error("SIMD128 missing from", bg);
  process.exit(1);
}
const sha256 = createHash("sha256").update(readFileSync(bg)).digest("hex");
assert.equal(engineSourceHash(), sourceSha256, "engine source changed during build; rebuild before packing");
writeFileSync(
  join(out, "integrity.json"),
  JSON.stringify({ file: "z_wasm_bg.wasm", sha256, sourceSha256, toolchain, mode: "multi-thread" }, null, 2) + "\n",
);
console.log("multicore wasm →", out, "(SIMD128)", sha256);
