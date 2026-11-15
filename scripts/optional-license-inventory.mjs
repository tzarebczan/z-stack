#!/usr/bin/env node
// Runtime dependency licenses for the optional Solana and two-leg swap packages.
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
const name = process.argv.find(value => ['solana', 'swaps'].includes(value));
assert.ok(name, 'Select solana or swaps');
const packageDir = join(root, 'packages', name);
const checksum = (text) => createHash("sha256").update(text).digest("hex");
const workspaceSeen = new Set();
const records = new Map(),
  notices = new Map();
function visit(name, from) {
  const request = createRequire(join(from, "package.json"));
  let dir;
  try {
    if (name.startsWith('@z-stack/')) dir = join(root, 'packages', name.slice('@z-stack/'.length));
    else dir = dirname(request.resolve(name + "/package.json"));
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
  if (pkg.name.startsWith('@z-stack/')) {
    if (workspaceSeen.has(id)) return;
    workspaceSeen.add(id);
    for (const dependency of Object.keys(pkg.dependencies ?? {}).sort()) visit(dependency, dir);
    return;
  }
  if (records.has(id)) return;
  if (!pkg.license && Array.isArray(pkg.licenses) && pkg.licenses.length === 1 && typeof pkg.licenses[0] === 'string') pkg.license = pkg.licenses[0];
  // This historical tarball omits package.json license; its retained LICENSE.md
  // dedicates original code to the public domain and identifies WHATWG-derived text.
  if (id === 'text-encoding-utf-8@1.0.2' && !pkg.license) pkg.license = 'Unlicense AND LicenseRef-WHATWG';
  assert.ok(
    ["LGPL-3.0-only", "0BSD", "Unlicense AND LicenseRef-WHATWG", "MIT", "Apache-2.0", "ISC", "BSD-2-Clause", "BSD-3-Clause", "(MIT OR Apache-2.0)", "(MIT AND CC-BY-3.0)"].includes(pkg.license),
    `Review new license ${id}: ${pkg.license}`,
  );
  const files = readdirSync(dir)
    .filter((file) => /^(LICENSE|LICENCE|COPYING|NOTICE)([._-]|$)/i.test(file))
    .sort();
  // Some historical npm tarballs retain their sole license declaration in README.
  if (!files.length) {
    const readme = readdirSync(dir).find(file => /^readme\.md$/i.test(file));
    if (readme && /(?:license|licence)[\s\S]{0,100}(?:MIT|ISC|Apache)/i.test(readFileSync(join(dir, readme), 'utf8'))) files.push(readme);
  }
  // tr46 0.0.3's npm package declares MIT but omits the upstream license file.
  const retained = id === 'tr46@0.0.3' ? join(root, 'licenses/solana/upstream/tr46-0.0.3-LICENSE.md') : null;
  if (!files.length && retained) files.push('UPSTREAM-LICENSE.md');
  assert.ok(files.length, `Missing retained legal text: ${id}`);
  const texts = files.map((file) => ({
    filename: file,
    text: readFileSync(file === "UPSTREAM-LICENSE.md" && retained ? retained : join(dir, file), "utf8").replace(/\r\n/g, "\n"),
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
for (const dependency of Object.keys(JSON.parse(readFileSync(join(packageDir, "package.json"))).dependencies ?? {}).sort()) visit(dependency, packageDir);
const dependencies = [...records.values()].sort((a, b) =>
  `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, "en"),
);
const outputs = {
  "NOTICE": `z-stack optional ${name} package\nOriginal z-stack code: Apache-2.0.\nThird-party runtime notices are retained in THIRD_PARTY_LICENSES.txt.\n`,
  "dependencies.json":
    JSON.stringify({ package: `@z-stack/${name}`, edges: "runtime", dependencies }, null, 2) + "\n",
  "LICENSES.txt":
    `z-stack optional ${name} package: third-party runtime license texts\nOriginal z-stack code: Apache-2.0. Third-party terms remain in force.\n` +
    dependencies
      .map(
        (row) =>
          `\n${"=".repeat(72)}\n${row.name} ${row.version} — ${row.license}\n${row.source}\n${notices.get(`${row.name}@${row.version}`)}`,
      )
      .join("\n"),
};
mkdirSync(join(root, "licenses/" + name), { recursive: true });
for (const [filename, contents] of Object.entries(outputs)) {
  const path = join(root, "licenses/" + name, filename);
  if (process.argv.includes("--check"))
    assert.equal(checksum(readFileSync(path, "utf8")), checksum(contents), `Stale optional package ${name}; regenerate and review`);
  else writeFileSync(path, contents);
}
console.log(`${name} license inventory: ${dependencies.length} runtime dependencies`);
