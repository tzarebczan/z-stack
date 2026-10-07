// Fingerprint compiler inputs, not Git state, so clean checkouts and source archives
// have the same provenance. Excludes build products, coordination files and secrets.
import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
export function engineSourceHash() {
  const walk = dir => readdirSync(join(root, dir), { withFileTypes: true }).flatMap(entry => {
    const path = `${dir}/${entry.name}`;
    if (entry.isDirectory()) return entry.name === "target" ? [] : walk(path);
    return /\.(rs|S)$/.test(entry.name) || entry.name === "Cargo.toml" ? [path] : [];
  });
  const inputs = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml",
    "scripts/build-wasm.mjs", "scripts/build-wasm-mt.mjs", "scripts/wasm-build-inputs.mjs",
    ...walk("crates/z-engine"), ...walk("crates/z-wasm"), ...walk("vendor/zakura")].sort();
  const hash = createHash("sha256");
  for (const path of inputs) hash.update(path).update("\0").update(readFileSync(join(root, path))).update("\0");
  return hash.digest("hex");
}
