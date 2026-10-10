import assert from "node:assert/strict";
import test from "node:test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, renameSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { docsOnly } from "./ci-changes.mjs";
import { ciPassed } from "./ci-gate.mjs";

function results(full) {
  return {
    changes: { result: "success", outputs: { full: String(full) } },
    native: { result: full ? "success" : "skipped" },
    sdk: { result: full ? "success" : "skipped" },
    browser: { result: full ? "success" : "skipped" },
    docs: { result: full ? "skipped" : "success" },
  };
}

test("only maintained public prose selects the lightweight route", () => {
  assert.equal(docsOnly(["README.md", "docs/SDK.md", "examples/browser-wallet/HOW-IT-WORKS.md", "packages/sdk/README.md"]), true);
  for (const path of ["Cargo.lock", "Cargo.toml", ".github/workflows/ci.yml", "scripts/check-docs.mjs", "crates/z-engine/src/lib.rs", "packages/sdk/src/wallet.ts", "examples/browser-wallet/src/main.ts", "docs/api/generated.md", "AGENTS.md", "vendor/zakura/README.md", "unknown.md"])
    assert.equal(docsOnly(["README.md", path]), false, path);
  assert.equal(docsOnly([]), false);
});

test("a code-to-docs rename cannot hide a removed engine source file", t => {
  const root = mkdtempSync(join(tmpdir(), "z-stack-ci-diff-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const git = (...args) => execFileSync("git", args, { cwd: root, stdio: ["ignore", "pipe", "pipe"] }).toString().trim();
  git("init"); git("config", "user.name", "Disposable fixture"); git("config", "user.email", "fixture@example.invalid");
  mkdirSync(join(root, "crates")); mkdirSync(join(root, "docs"));
  writeFileSync(join(root, "crates", "engine.rs"), "fixture source");
  git("add", "."); git("-c", "commit.gpgsign=false", "commit", "-m", "Fixture");
  const base = git("rev-parse", "HEAD");
  renameSync(join(root, "crates", "engine.rs"), join(root, "docs", "engine.md"));
  git("add", "."); git("-c", "commit.gpgsign=false", "commit", "-m", "Fixture rename");
  const paths = git("diff", "--no-renames", "--name-only", "-z", base, "HEAD").split("\0").filter(Boolean);
  assert.equal(docsOnly(paths), false);
  const output = join(root, "outputs");
  const classify = (event, revision) => {
    writeFileSync(output, "");
    execFileSync(process.execPath, [resolve(import.meta.dirname, "ci-changes.mjs")], {
      cwd: root,
      env: { ...process.env, CI_EVENT_NAME: event, CI_BASE_SHA: revision, GITHUB_OUTPUT: output },
    });
    return readFileSync(output, "utf8");
  };
  assert.equal(classify("pull_request", base), "full=true\n");
  const docsBase = git("rev-parse", "HEAD");
  writeFileSync(join(root, "docs", "engine.md"), "Updated prose");
  git("add", "docs/engine.md"); git("-c", "commit.gpgsign=false", "commit", "-m", "Fixture docs");
  assert.equal(classify("pull_request", docsBase), "full=false\n");
  assert.equal(classify("push", docsBase), "full=true\n");
});

test("the required gate accepts completed full and intentionally docs-only routes", () => {
  assert.equal(ciPassed(results(true)), true);
  assert.equal(ciPassed(results(false)), true);
});

test("failures, cancellations and accidental skips cannot satisfy the required check", () => {
  for (const full of [true, false]) {
    for (const job of ["changes", ...(full ? ["native", "sdk", "browser"] : ["docs"])]) {
      for (const status of ["failure", "cancelled", "skipped", undefined]) {
        const needs = results(full); needs[job].result = status;
        assert.equal(ciPassed(needs), false, `${full}/${job}/${status}`);
      }
    }
  }
  const needs = results(true); needs.changes.outputs.full = "";
  assert.equal(ciPassed(needs), false);
  assert.equal(ciPassed({}), false);
});

test("the workflow gate CLI exits unsuccessfully for a cancelled browser matrix", () => {
  const script = resolve(import.meta.dirname, "ci-gate.mjs");
  for (const full of [true, false]) execFileSync(process.execPath, [script], {
    env: { ...process.env, CI_NEEDS: JSON.stringify(results(full)) },
  });
  const cancelled = results(true); cancelled.browser.result = "cancelled";
  assert.throws(() => execFileSync(process.execPath, [script], {
    env: { ...process.env, CI_NEEDS: JSON.stringify(cancelled) }, stdio: "pipe",
  }), error => error.status === 1);
});
