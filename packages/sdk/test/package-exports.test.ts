import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

// Integrators import the built packages with plain Node (no bundler, no
// source condition, no tsx); every relative import must carry its extension.
test("built packages import in plain Node", (t) => {
  for (const pkg of ["core", "passkey", "sdk"]) {
    if (!existsSync(join(root, pkg, "dist", "index.js"))) {
      t.skip(`build packages first (pnpm --filter @z-stack/${pkg} build)`);
      return;
    }
  }
  const imports: Array<[string, string]> = [
    ["sdk", "@z-stack/sdk"],
    ["sdk", "@z-stack/sdk/lab"],
    ["sdk", "@z-stack/sdk/vite"],
    ["sdk", "@z-stack/sdk/services"],
    ["sdk", "@z-stack/sdk/diagnostics"],
        ["sdk", "@z-stack/sdk/core"],
    ["sdk", "@z-stack/sdk/hardware"],
    ["sdk", "@z-stack/sdk/native"],
    ["sdk", "@z-stack/sdk/engine"],
    ["passkey", "@z-stack/passkey"],
    ["core", "@z-stack/core"],
  ];
  for (const [pkg, specifier] of imports) {
    const out = execFileSync(
      process.execPath,
      ["--input-type=module", "-e", `const m = await import(${JSON.stringify(specifier)}); console.log(Object.keys(m).length)`],
      // A clean environment: the test runner's own --conditions/--import flags must not leak in.
      { cwd: join(root, pkg), env: { PATH: process.env.PATH ?? "" }, encoding: "utf8" },
    );
    assert.ok(Number(out.trim()) > 0, `${specifier} exports nothing`);
  }
});

test("adapter checks belong to lab and no testing subpath is published", () => {
  execFileSync(process.execPath, ["--input-type=module", "-e", `
    import assert from 'node:assert/strict';
    const sdk = await import('@z-stack/sdk');
    const lab = await import('@z-stack/sdk/lab');
    assert.equal(typeof lab.checkWalletStorageAdapter, 'function');
    assert.equal(typeof lab.checkVaultStoreAdapter, 'function');
    assert.equal('checkWalletStorageAdapter' in sdk, false);
    await assert.rejects(import('@z-stack/sdk/testing'), {code:'ERR_PACKAGE_PATH_NOT_EXPORTED'});
  `], {cwd:join(root,"sdk"),env:{PATH:process.env.PATH??""},stdio:"pipe"});
});
