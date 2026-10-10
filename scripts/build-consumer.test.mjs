import assert from "node:assert/strict";
import test from "node:test";
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildConsumer } from "./build-consumer.mjs";

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "z-stack-build-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const cache = join(root, "cache");
  const app = name => {
    const dir = join(root, name); mkdirSync(dir);
    writeFileSync(join(dir, "package.json"), JSON.stringify({ scripts: { build: "node build.mjs" } }));
    writeFileSync(join(dir, "build.mjs"), `import {mkdirSync,writeFileSync} from 'node:fs';
      if (process.env.FAIL_BUILD) throw new Error('restore attempted compilation');
      mkdirSync('dist'); writeFileSync('dist/index.html','built consumer');
      writeFileSync('next-env.d.ts','generated declaration');`);
    return dir;
  };
  const env = mode => ({ Z_STACK_CONSUMER_BUILDS: cache, Z_STACK_CONSUMER_BUILD_MODE: mode });
  return { cache, app, env };
}
test("prepared builds restore into a fresh consumer without invoking its compiler", t => {
  const { cache, app, env } = fixture(t);
  buildConsumer(app("prepare"), { env: env("prepare") });
  const restored = app("restore");
  buildConsumer(restored, { env: { ...env("restore"), FAIL_BUILD: "1" } });
  assert.equal(readFileSync(join(restored, "dist/index.html"), "utf8"), "built consumer");
  assert.equal(readFileSync(join(restored, "next-env.d.ts"), "utf8"), "generated declaration");
  assert.equal(readdirSync(cache).length, 1);
});
test("changed sources or build-time isolation settings cannot reuse a prepared build", t => {
  const { app, env } = fixture(t);
  buildConsumer(app("prepare"), { env: env("prepare") });
  const changed = app("changed"); writeFileSync(join(changed, "new-source.ts"), "export {}");
  assert.throws(() => buildConsumer(changed, { env: env("restore") }), /No prepared consumer build/);
  assert.throws(() => buildConsumer(app("isolation"), { env: { ...env("restore"), Z_STACK_ISOLATION: "off" } }), /No prepared consumer build/);
});
test("altered, missing or unexpected compiled files fail before replacing a consumer's output", t => {
  const { cache, app, env } = fixture(t);
  buildConsumer(app("prepare"), { env: env("prepare") });
  const files = join(cache, readdirSync(cache)[0], "files");
  const restored = app("restore"); mkdirSync(join(restored, "dist")); writeFileSync(join(restored, "dist/old"), "preserved");
  writeFileSync(join(files, "dist/index.html"), "changed");
  assert.throws(() => buildConsumer(restored, { env: env("restore") }), /integrity failed/);
  assert.equal(readFileSync(join(restored, "dist/old"), "utf8"), "preserved");
  rmSync(join(files, "dist/index.html"));
  assert.throws(() => buildConsumer(restored, { env: env("restore") }), /file set changed/);
  writeFileSync(join(files, "extra.js"), "extra");
  assert.throws(() => buildConsumer(restored, { env: env("restore") }), /file set changed/);
});
