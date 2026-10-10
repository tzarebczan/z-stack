import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const ignoredInputs = new Set(["node_modules", "dist", ".next", ".git", "ssr.mjs"]);
function files(root, skip = () => false, path = "") {
  return readdirSync(join(root, path), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name)).flatMap(entry => {
    const name = path ? `${path}/${entry.name}` : entry.name;
    if (skip(name)) return [];
    assert.ok(!entry.isSymbolicLink(), `Build artifact contains a symbolic link: ${name}`);
    return entry.isDirectory() ? files(root, skip, name) : [name];
  });
}
const digest = bytes => createHash("sha256").update(bytes).digest("hex");
function inputs(app, env) {
  const paths = files(app, path => ignoredInputs.has(path.split("/")[0]) || path.endsWith(".tsbuildinfo"));
  const settings = Object.entries(env).filter(([name]) => /^(VITE_|NEXT_PUBLIC_)/.test(name)
    || ["Z_STACK_ISOLATION", "NODE_ENV", "NODE_OPTIONS"].includes(name)).filter(([, value]) => value).sort();
  return digest(JSON.stringify({ version: 1, node: process.version, platform: process.platform, arch: process.arch,
    settings, files: paths.map(path => [path, digest(readFileSync(join(app, path)))]) }));
}

/** Cache only same-run compiled consumers; every caller still installs its own archives. */
export function buildConsumer(app, options = {}) {
  const env = { ...process.env, ...options.env };
  const mode = env.Z_STACK_CONSUMER_BUILD_MODE;
  const cache = env.Z_STACK_CONSUMER_BUILDS;
  if (mode) {
    assert.ok(["prepare", "restore"].includes(mode) && cache, "Consumer build reuse needs a mode and directory");
  }
  const key = mode ? inputs(app, env) : undefined;
  const entry = mode ? join(resolve(cache), key) : undefined;
  if (mode === "restore") {
    assert.ok(existsSync(join(entry, "manifest.json")), `No prepared consumer build matches these sources and settings: ${key}`);
    const manifest = JSON.parse(readFileSync(join(entry, "manifest.json"), "utf8"));
    assert.equal(manifest.key, key, "Consumer sources do not match the prepared build");
    assert.deepEqual(files(join(entry, "files")), manifest.files.map(([path]) => path), "Consumer build file set changed");
    for (const [path, hash] of manifest.files) {
      assert.ok(/^(dist\/|\.next\/|tsconfig\.json$|next-env\.d\.ts$)/.test(path)
        && !path.split("/").includes(".."), "Invalid consumer output path");
      assert.equal(digest(readFileSync(join(entry, "files", path))), hash, `Consumer build integrity failed: ${path}`);
    }
    assert.ok(manifest.files.some(([path]) => path.startsWith("dist/") || path.startsWith(".next/")), "Consumer build is empty");
    for (const output of ["dist", ".next"]) rmSync(join(app, output), { recursive: true, force: true });
    cpSync(join(entry, "files"), app, { recursive: true });
    console.log(`Reusing verified consumer build ${key.slice(0, 12)}`);
    return;
  }
  const result = spawnSync("npm", ["run", "build"], { cwd: app, stdio: "inherit", env,
    shell: process.platform === "win32" });
  assert.equal(result.status, 0, "Consumer production build failed");
  if (mode === "prepare") {
    assert.ok(!existsSync(entry), "Consumer build was prepared twice");
    const output = join(entry, "files");
    mkdirSync(output, { recursive: true });
    for (const dir of ["dist", ".next"]) if (existsSync(join(app, dir))) {
      cpSync(join(app, dir), join(output, dir), { recursive: true,
        filter: path => path !== join(app, ".next", "cache") });
    }
    // Next generates its declaration file and can update the TypeScript config.
    for (const path of ["tsconfig.json", "next-env.d.ts"]) if (existsSync(join(app, path)))
      cpSync(join(app, path), join(output, path));
    const outputs = files(output);
    assert.ok(outputs.some(path => path.startsWith("dist/") || path.startsWith(".next/")), "Consumer build is empty");
    writeFileSync(join(entry, "manifest.json"), JSON.stringify({ key,
      files: outputs.map(path => [path, digest(readFileSync(join(output, path)))]) }, null, 2) + "\n");
    console.log(`Prepared consumer build ${key.slice(0, 12)}`);
  }
}
