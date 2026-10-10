import assert from "node:assert/strict";
import test from "node:test";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { reuseConsumerArchives } from "./consumer-archives.mjs";

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "z-stack-archive-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  writeFileSync(join(root, "package.json"), JSON.stringify({ version: "0.1.0-alpha.fixture" }));
  const source = join(root, "source"), destination = join(root, "consumer");
  mkdirSync(source);
  const file = name => `z-stack-${name}-0.1.0-alpha.fixture.tgz`;
  for (const name of ["core", "passkey", "sdk", "base"]) writeFileSync(join(source, file(name)), name);
  return { root, source, destination, file };
}

test("a reused SDK set is isolated and does not accidentally install Base", t => {
  const { root, source, destination, file } = fixture(t);
  assert.equal(reuseConsumerArchives(root, destination, ["core", "passkey", "sdk"], [`--archives=${source}`]), true);
  assert.deepEqual(readdirSync(destination).sort(), [file("core"), file("passkey"), file("sdk")].sort());
  writeFileSync(join(destination, file("sdk")), "consumer changed its copy");
  assert.equal(readFileSync(join(source, file("sdk")), "utf8"), "sdk");
});

test("Base alone and combined consumers select their own archive sets", t => {
  const { root, source, destination, file } = fixture(t);
  assert.equal(reuseConsumerArchives(root, destination, ["base"], [`--archives=${source}`]), true);
  assert.deepEqual(readdirSync(destination), [file("base")]);
  const combined = join(root, "combined");
  reuseConsumerArchives(root, combined, ["core", "passkey", "sdk", "base"], [`--archives=${source}`]);
  assert.equal(readdirSync(combined).length, 4);
});

test("an incomplete or different-version set fails before any consumer files are copied", t => {
  const { root, source, destination, file } = fixture(t);
  rmSync(join(source, file("sdk")));
  writeFileSync(join(source, "z-stack-sdk-0.1.0-alpha.old.tgz"), "old");
  assert.throws(() => reuseConsumerArchives(root, destination, ["core", "passkey", "sdk"], [`--archives=${source}`]), /Missing prebuilt archive.*sdk/);
  assert.equal(existsSync(destination), false);
});

test("local packing remains the default and malformed reuse options never fall back", t => {
  const { root, source, destination } = fixture(t);
  assert.equal(reuseConsumerArchives(root, destination, ["sdk"], []), false);
  assert.equal(existsSync(destination), false);
  for (const args of [["--archives="], ["--archives", source], [`--archives=${source}`, `--archives=${source}`]])
    assert.throws(() => reuseConsumerArchives(root, destination, ["sdk"], args), /Use/);
});
