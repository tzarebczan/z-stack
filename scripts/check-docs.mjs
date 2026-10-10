#!/usr/bin/env node
// Check maintained developer entrypoints and forbid unexplained legacy labels.
import assert from "node:assert/strict";
import { markdownAnchors } from "./doc-anchors.mjs";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
function documents(directory) {
  return readdirSync(join(root, directory), { withFileTypes: true }).flatMap(entry => {
    const path = `${directory}/${entry.name}`;
    if (entry.isDirectory()) return entry.name === "api" ? [] : documents(path);
    return entry.name.endsWith(".md") ? [path] : [];
  });
}
const entries = ["README.md", "CHANGELOG.md", "CONTRIBUTING.md", "SECURITY.md", "AGENTS.md", ...documents("docs"), "infra/compose/README.md",
  "packages/sdk/README.md", "packages/core/README.md", "packages/passkey/README.md", "packages/base/README.md", ...readdirSync(join(root, "examples")).filter(name => existsSync(join(root, "examples", name, "README.md"))).flatMap(name => ["README.md", "HOW-IT-WORKS.md"].filter(file => existsSync(join(root, "examples", name, file))).map(file => `examples/${name}/${file}`)), "vendor/zakura/README.md"];
let count = 0;
const portable = new Set([...['sdk', 'core', 'passkey', 'base'].map(name => `packages/${name}/README.md`),
  ...Object.keys(JSON.parse(readFileSync(join(root, 'examples/templates.json'), 'utf8'))).flatMap(name => ["README.md", "HOW-IT-WORKS.md"].filter(file => existsSync(join(root, "examples", name, file))).map(file => `examples/${name}/${file}`))]);
for (const name of entries) {
  const path = join(root, name);
  const text = readFileSync(path, "utf8");
  assert.ok(!/\boption[ _-]c\b/i.test(text), `${name}: replace opaque scan terminology`);
  // Ignore fenced examples: placeholder paths in code are not documentation links.
  const prose = text.replace(/```[\s\S]*?```/g, "");
  for (const match of prose.matchAll(/\[[^\]]*\]\(([^\s)]+)(?:\s+"[^"]*")?\)/g)) {
    let target = match[1].replace(/^<|>$/g, "");
    if (portable.has(name) && !/^[a-z]+:|^\/|^#/i.test(target)) {
      const local = resolve(dirname(path), decodeURIComponent(target.split('#')[0]));
      assert.ok(local.startsWith(dirname(path) + sep), `${name}: installed README link escapes its package/app: ${target}; use a repository URL`);
    }
    const own = /^https:\/\/github\.com\/tzarebczan\/z-stack\/(?:blob|tree)\/main\/(.+)$/.exec(target);
    if (own) target = resolve(root, own[1]);
    else if (/^[a-z]+:|^\//i.test(target)) continue;
    const [file, fragment] = target.split("#");
    const linked = file ? resolve(dirname(path), decodeURIComponent(file)) : path;
    assert.ok(existsSync(linked), `${name}: broken link ${match[1]}`);
    if (fragment && linked.endsWith(".md")) {
      assert.ok(markdownAnchors(readFileSync(linked, "utf8")).has(decodeURIComponent(fragment)),
        `${name}: broken heading link ${match[1]}`);
    }
    count++;
  }
}
console.log(`${entries.length} developer documents: ${count} local links verified`);
