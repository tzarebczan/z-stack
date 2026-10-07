#!/usr/bin/env node
// Build a package's dist/: JS + .d.ts from src/ with tsc. For the SDK, also
// copy the WASM artifacts and point worker URLs at the emitted .js files.
//
//   node scripts/build-package.mjs <core|passkey|sdk|base>
import { basePackageInputs } from "./base-package-inputs.mjs";
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const name = process.argv[2];
if (!["core", "passkey", "sdk", "base"].includes(name ?? "")) {
  console.error("usage: node scripts/build-package.mjs <core|passkey|sdk|base>");
  process.exit(2);
}
const pkg = join(root, "packages", name);
const dist = join(pkg, "dist");

if (name === "sdk") {
  for (const dep of ["core", "passkey"]) {
    if (!existsSync(join(root, "packages", dep, "dist", "index.d.ts"))) {
      console.error(`missing packages/${dep}/dist/index.d.ts: run \`pnpm --filter @z-stack/${dep} build\` first`);
      process.exit(1);
    }
  }
  for (const dir of ["generated", "generated-mt"]) {
    for (const file of ["z_wasm.js", "z_wasm_bg.wasm", "integrity.json"]) {
      if (!existsSync(join(pkg, "src", dir, file))) {
        console.error(`missing src/${dir}/${file}: run \`pnpm build:wasm\` and \`pnpm build:wasm:mt\` first`);
        process.exit(1);
      }
    }
  }
}

rmSync(dist, { recursive: true, force: true });
const tsconfig = join(pkg, "tsconfig.build.json");
writeFileSync(
  tsconfig,
  JSON.stringify(
    {
      extends: "./tsconfig.json",
      compilerOptions: {
        noEmit: false,
        declaration: true,
        sourceMap: true,
        inlineSources: true,
        outDir: "dist",
        rootDir: "src",
        // Dist .d.ts, not the @z-stack/source entry. That condition pulls
        // sibling src/ into this program and tsc rejects it under rootDir.
        customConditions: [],
      },
      include: ["src/**/*.ts"],
      exclude: ["src/generated/**", "src/generated-mt/**"],
    },
    null,
    2,
  ),
);
// TypeScript's JS entry through this Node: pnpm's .bin/tsc is a shell shim
// that spawnSync cannot launch on Windows.
let tscJs;
for (const from of [pkg, root]) {
  try {
    tscJs = createRequire(join(from, "package.json")).resolve("typescript/bin/tsc");
    break;
  } catch {
    /* try the next location */
  }
}
if (!tscJs) {
  rmSync(tsconfig, { force: true });
  console.error("typescript is not installed (pnpm install)");
  process.exit(1);
}
const run = spawnSync(process.execPath, [tscJs, "-p", tsconfig], { cwd: pkg, stdio: "inherit" });
rmSync(tsconfig, { force: true });
if (run.error) {
  console.error(`could not run tsc: ${run.error.message}`);
  process.exit(1);
}
if (run.status !== 0) process.exit(run.status ?? 1);

// Node's ESM loader needs file extensions; the sources import extensionlessly
// (bundler resolution). Qualify every relative specifier in the output.
const emitted = (dir) =>
  readdirSync(dir).flatMap((e) => {
    const p = join(dir, e);
    if (statSync(p).isDirectory()) return e.startsWith("generated") ? [] : emitted(p);
    return /\.(js|d\.ts)$/.test(e) ? [p] : [];
  });
const qualify = (file, spec) => {
  if (/\.(m?js|json|wasm)$/.test(spec)) return spec;
  const target = resolve(dirname(file), spec);
  if (existsSync(`${target}.js`) || existsSync(`${target}.d.ts`)) return `${spec}.js`;
  if (existsSync(join(target, "index.js")) || existsSync(join(target, "index.d.ts"))) return `${spec}/index.js`;
  return spec;
};
for (const file of emitted(dist)) {
  const text = readFileSync(file, "utf8");
  const next = text.replace(
    /(\bfrom\s*|\bimport\s*\(\s*|\bimport\s+)(["'])(\.{1,2}\/[^"']+?)\2/g,
    (_m, lead, quote, spec) => `${lead}${quote}${qualify(file, spec)}${quote}`,
  );
  if (next !== text) writeFileSync(file, next);
}

if (name === "sdk") {
  // WASM + wasm-bindgen glue, including the Rayon worker snippets.
  const keep = /^(z_wasm\.js|z_wasm\.d\.ts|z_wasm_bg\.wasm|z_wasm_bg\.wasm\.d\.ts|integrity\.json|package\.json|index\.js|snippets)$/;
  for (const dir of ["generated", "generated-mt"]) {
    for (const entry of readdirSync(join(pkg, "src", dir))) {
      if (keep.test(entry)) cpSync(join(pkg, "src", dir, entry), join(dist, dir, entry), { recursive: true });
    }
  }
  // Worker URLs name the .ts sources; the emitted files are .js.
  const walk = (dir) =>
    readdirSync(dir).flatMap((e) => {
      const p = join(dir, e);
      return statSync(p).isDirectory() ? (e.startsWith("generated") ? [] : walk(p)) : p.endsWith(".js") ? [p] : [];
    });
  for (const file of walk(dist)) {
    const text = readFileSync(file, "utf8");
    const next = text.replace(/(new URL\(["']\.\/[\w.-]+\.worker)\.ts(["'])/g, "$1.js$2");
    if (next !== text) writeFileSync(file, next);
  }
  if (!readFileSync(join(dist, "wasm-client-workers.js"), "utf8").includes('new URL("./scan.worker.js"')) {
    console.error("worker URL rewrite failed");
    process.exit(1);
  }
}
if (name === "base") writeFileSync(join(dist, "build-inputs.json"), JSON.stringify({ sha256: basePackageInputs(pkg) }) + "\n");

// Base has its own runtime inventory; it ships no WASM or Rust dependencies.
cpSync(join(root, "LICENSE"), join(pkg, "LICENSE"));
for (const [target, source] of [["NOTICE", "NOTICE"], ["THIRD_PARTY_LICENSES.txt", "LICENSES.txt"],
  ["THIRD_PARTY_DEPENDENCIES.json", "dependencies.json"]]) {
  const from = name === "base" ? join(root, "licenses/base", source) :
    target === "THIRD_PARTY_DEPENDENCIES.json" ? join(root, "licenses/WASM-dependencies.json") : join(root, target);
  cpSync(from, join(pkg, target));
}
console.log(`built packages/${name}/dist`);
