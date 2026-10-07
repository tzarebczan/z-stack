#!/usr/bin/env node
// Offline runtime dependency inventory for the optional Base package, independent of WASM.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const checksum = (text) => createHash("sha256").update(text).digest("hex");
const records = new Map(),
  notices = new Map();
function visit(name, from) {
  const request = createRequire(join(from, "package.json"));
  let dir;
  try {
    dir = dirname(request.resolve(name + "/package.json"));
  } catch {
    dir = dirname(request.resolve(name));
    while (
      !existsSync(join(dir, "package.json")) ||
      JSON.parse(readFileSync(join(dir, "package.json"))).name !== name
    ) {
      const parent = dirname(dir);
      assert.notEqual(parent, dir, `Cannot locate ${name}`);
      dir = parent;
    }
  }
  dir = realpathSync(dir);
  const pkg = JSON.parse(readFileSync(join(dir, "package.json"))),
    id = `${pkg.name}@${pkg.version}`;
  if (records.has(id)) return;
  assert.ok(
    ["MIT", "Apache-2.0"].includes(pkg.license),
    `Review new license ${id}: ${pkg.license}`,
  );
  const files = readdirSync(dir)
    .filter((file) => /^(LICENSE|LICENCE|COPYING|NOTICE)(\.|$)/i.test(file))
    .sort();
  assert.ok(files.length, `Missing retained legal text: ${id}`);
  const texts = files.map((file) => ({
    filename: file,
    text: readFileSync(join(dir, file), "utf8").replace(/\r\n/g, "\n"),
  }));
  records.set(id, {
    name: pkg.name,
    version: pkg.version,
    license: pkg.license,
    source: `https://registry.npmjs.org/${pkg.name}/-/${pkg.name.split("/").at(-1)}-${pkg.version}.tgz`,
    legalFiles: texts.map(({ filename, text }) => ({ filename, sha256: checksum(text) })),
  });
  notices.set(id, texts.map(({ filename, text }) => `${filename}\n${text}`).join("\n"));
  for (const dependency of Object.keys(pkg.dependencies ?? {}).sort()) visit(dependency, dir);
}
visit("viem", join(root, "packages/base"));
const dependencies = [...records.values()].sort((a, b) =>
  `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, "en"),
);
const outputs = {
  "dependencies.json":
    JSON.stringify({ package: "@z-stack/base", edges: "runtime", dependencies }, null, 2) + "\n",
  "LICENSES.txt":
    "z-stack optional Base package: third-party runtime license texts\nOriginal z-stack code: Apache-2.0. Third-party terms remain in force.\n" +
    dependencies
      .map(
        (row) =>
          `\n${"=".repeat(72)}\n${row.name} ${row.version} — ${row.license}\n${row.source}\n${notices.get(`${row.name}@${row.version}`)}`,
      )
      .join("\n"),
};
mkdirSync(join(root, "licenses/base"), { recursive: true });
for (const [name, contents] of Object.entries(outputs)) {
  const path = join(root, "licenses/base", name);
  if (process.argv.includes("--check"))
    assert.equal(readFileSync(path, "utf8"), contents, `Stale Base ${name}; regenerate and review`);
  else writeFileSync(path, contents);
}
console.log(`Base license inventory: ${dependencies.length} runtime dependencies`);
