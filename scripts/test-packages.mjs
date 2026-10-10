#!/usr/bin/env node
import { reuseConsumerArchives } from "./consumer-archives.mjs";
// Real external-consumer check. No workspace links, TS source aliases, or SDK stubs.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createExample } from "./create-example.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const scratch = mkdtempSync(join(tmpdir(), "z-stack-consumer-"));
const app = join(scratch, "app");
const archives = join(scratch, "archives");
let playwright;
const browserNames = (process.argv.find(arg => arg.startsWith("--browsers="))?.slice(11) ?? "chromium").split(",");
assert.ok(browserNames.every(name => ["chromium", "firefox", "webkit"].includes(name)), "unsupported browser selection");
function run(command, args, cwd = app) {
  const result = spawnSync(command, args, { cwd, stdio: "inherit", shell: process.platform === "win32",
    env: { ...process.env, NODE_OPTIONS: "" } });
  assert.equal(result.status, 0, `${command} ${args.join(" ")} failed`);
}
try {
  if (!reuseConsumerArchives(root, archives, ["core", "passkey", "sdk"]))
    run(process.execPath, [join(root, "scripts", "pack-sdk.mjs"), archives], root);
  // Installing just the SDK must work with an unreachable registry and no cache.
  const standalone = join(scratch, "standalone");
  const { mkdirSync } = await import("node:fs");
  mkdirSync(standalone);
  writeFileSync(join(standalone, "package.json"), JSON.stringify({private:true, type:"module"}));
  const sdkArchive = join(archives, `z-stack-sdk-${JSON.parse(readFileSync(join(root, "package.json"))).version}.tgz`);
  run("npm", ["install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund",
    "--cache", join(scratch, "empty-cache"), "--registry", "http://127.0.0.1:1", sdkArchive], standalone);
  run(process.execPath, ["--input-type=module", "-e", `
    import assert from 'node:assert/strict';
    const sdk = await import('@z-stack/sdk');
    const services = await import('@z-stack/sdk/services');
    const lab = await import('@z-stack/sdk/lab');
    assert.equal(typeof sdk.createWallet, 'function');
    assert.equal(sdk.WalletError.fromMessage('propose_transfer: Must scan blocks first').code, 'sync_required');
    assert.equal(sdk.WalletError.fromMessage('invalid recovery phrase: private detail').code, 'invalid_recovery_phrase');
    assert.equal(await services.memoryVaultStore().get('missing'), undefined);
    assert.equal(typeof lab.checkVaultStoreAdapter, 'function');
    console.log('SDK archive alone: root/services/lab import with no registry or cache');
  `], standalone);
  createExample("browser-wallet", app, archives);
  const tarballs = readdirSync(archives).filter(file => file.endsWith(".tgz")).sort();
  assert.equal(tarballs.length, 3);
  run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund", ...tarballs.map(file => join(archives, file))]);
  for (const name of ["core", "passkey", "sdk"]) {
    const dir = join(app, "node_modules", "@z-stack", name);
    const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    assert.ok(!existsSync(join(dir, "src")), `${name}: unbuilt sources shipped`);
    for (const entry of Object.values(pkg.exports)) {
      assert.ok(!("@z-stack/source" in entry), `${name}: repository source condition leaked`);
      for (const path of Object.values(entry)) assert.ok(existsSync(join(dir, path)), `${name}: dangling export ${path}`);
    }
    for (const value of Object.values(pkg.dependencies ?? {})) {
      assert.ok(!/^(file:|link:|workspace:)/.test(value), `${name}: nonportable dependency ${value}`);
    }
    for (const file of ["README.md", "LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.txt", "THIRD_PARTY_DEPENDENCIES.json"]) assert.ok(existsSync(join(dir, file)), `${name}: missing ${file}`);
  }
  const dist = join(app, "node_modules", "@z-stack", "sdk", "dist");
  // A diagnostics-only app must not pull in the engine, storage or passkeys.
  // Inspect the actual installed dependency graph, not just bundle byte size.
  const { buildSync } = createRequire(join(app, "package.json"))("esbuild");
  const diagnosticBundle = buildSync({ entryPoints: [join(dist, "diagnostics.js")],
    bundle: true, platform: "browser", format: "esm", write: false, metafile: true });
  const diagnosticInputs = Object.keys(diagnosticBundle.metafile.inputs);
  assert.ok(!diagnosticInputs.some(file => /(?:wasm-client|runtime|wallet-storage|storage|generated|passkey|create-wallet)[/\\.]/.test(file)),
    `diagnostics imported wallet machinery: ${diagnosticInputs.join(", ")}`);
  console.log(`Installed diagnostics: ${diagnosticInputs.length} lightweight modules, ${diagnosticBundle.outputFiles[0].contents.length} bytes`);
  for (const kind of ["generated", "generated-mt"]) {
    const bytes = readFileSync(join(dist, kind, "z_wasm_bg.wasm"));
    const manifest = JSON.parse(readFileSync(join(dist, kind, "integrity.json"), "utf8"));
    assert.equal(createHash("sha256").update(bytes).digest("hex"), manifest.sha256);
  }
  run(process.execPath, ["--input-type=module", "-e", `
    import assert from 'node:assert/strict';
    import { readFileSync } from 'node:fs';
    const originalFetch = globalThis.fetch;
    globalThis.fetch = () => { throw new Error('Account/backup network request during import'); };
    const oldIdb = Object.getOwnPropertyDescriptor(globalThis, 'indexedDB');
    Object.defineProperty(globalThis, 'indexedDB', { configurable: true, get() { throw new Error('Storage opened during SSR import'); } });
    const oldWorker = Object.getOwnPropertyDescriptor(globalThis, 'Worker');
    Object.defineProperty(globalThis, 'Worker', { configurable: true, value: class { constructor() { throw new Error('Worker started during SSR import'); } } });
    const services = await import('@z-stack/sdk/services');
    const sdk = await import('@z-stack/sdk');
    await import('@z-stack/sdk/diagnostics');
    const lab = await import('@z-stack/sdk/lab');
    assert.equal(typeof lab.checkWalletStorageAdapter, 'function');
    assert.equal(typeof lab.checkVaultStoreAdapter, 'function');
    assert.equal('checkWalletStorageAdapter' in sdk, false);
    await assert.rejects(import('@z-stack/sdk/testing'), { code: 'ERR_PACKAGE_PATH_NOT_EXPORTED' });
    await import('@z-stack/sdk/native');
    await import('@z-stack/sdk/hardware');
    await import('@z-stack/sdk/core');
    if (oldIdb) Object.defineProperty(globalThis, 'indexedDB', oldIdb); else delete globalThis.indexedDB;
    if (oldWorker) Object.defineProperty(globalThis, 'Worker', oldWorker); else delete globalThis.Worker;
    assert.equal(typeof services.createPasskeyVault, 'function');
    const store = services.memoryVaultStore();
    assert.equal(await store.get('missing'), undefined);
    globalThis.fetch = originalFetch;
    const core = await import('@z-stack/core');
    const passkey = await import('@z-stack/passkey');
    const plugin = await import('@z-stack/sdk/vite');
    await import('@z-stack/sdk/lab');
    assert.equal(sdk.parseZecToZatoshis('0.25'), 25000000n);
    assert.equal(core.formatZatoshis(25000000n), '0.25000000');
    assert.equal(typeof passkey.createPasskeyVault, 'function');
    assert.equal(plugin.zStack().config().worker.format, 'es');
    const engine = await import('@z-stack/sdk/engine');
    await engine.initialize({ wasmModule: readFileSync('./node_modules/@z-stack/sdk/dist/generated/z_wasm_bg.wasm'), prewarmProveWorker: false });
    const account = sdk.deriveAccount(engine.generateMnemonic(), 'regtest');
    assert.ok(sdk.isValidAddress(account.unifiedAddress, 'regtest'));
    console.log('Installed ESM exports and production engine verified');
  `]);
  // Compile the exact maintained snippets with the installed published subpaths.
  // Source aliases must not hide missing root exports in a copied guide.
  for (const [file, minimum] of [["SERVICES.md", 3], ["SDK.md", 1]]) {
    const guide = readFileSync(join(root, "docs", file), "utf8");
    const examples = [...guide.matchAll(/<!-- sdk-example: ([a-z-]+) -->\s*```ts\n([\s\S]*?)\n```/g)];
    assert.ok(examples.length >= minimum, `missing checked ${file} examples`);
    for (const [, name, source] of examples) writeFileSync(join(app, "src", `guide-${file.slice(0, -3)}-${name}.ts`), source);
  }
  run("npm", ["run", "build"]);
  const { verifyVitePreview } = await import("./test-vite-preview.mjs");
  await verifyVitePreview(app);
  const assets = readdirSync(join(app, "dist", "assets"));
  assert.ok(assets.filter(file => file.endsWith(".wasm")).length >= 2, "both engine variants must be emitted");
  for (const worker of ["scan.worker", "prove.worker", "workerHelpers"]) {
    assert.ok(assets.some(file => file.includes(worker) && file.endsWith(".js")), `missing ${worker}`);
  }
  if (process.argv.includes("--browser") || process.argv.includes("--funded-regtest")) {
    // Browser acceptance uses the SDK workspace's pinned test dependency.
    playwright = createRequire(join(root, "package.json"))("playwright");
    const { verifyBrowserPackages } = await import("./test-packages-browser.mjs");
    if (process.argv.includes("--browser")) for (const browserName of browserNames) await verifyBrowserPackages(app, playwright, browserName);
    if (process.argv.includes("--funded-regtest")) {
      const { verifyFundedBrowser } = await import("./test-funded-browser.mjs");
      await verifyFundedBrowser(app, playwright);
    }
  }
  for (const example of ["react-wallet", "local-passkey", "remote-backup", "next-wallet"]) {
    const consumer = join(scratch, example);
    createExample(example, consumer, archives);
    run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund", ...tarballs.map(file => join(archives, file))], consumer);
    run("npm", ["run", "build"], consumer);
    if (example === "remote-backup") run("npm", ["test"], consumer);
    if (example === "react-wallet") {
      // Bundle only the component for Node, then prove rendering cannot touch
      // browser APIs. This does not claim a Next.js/Remix deployment was tested.
      const require = createRequire(join(consumer, "package.json"));
      const { buildSync } = require("esbuild");
      buildSync({ entryPoints: [join(consumer, "src", "WalletPanel.tsx")], outfile: join(consumer, "ssr.mjs"),
        bundle: true, platform: "node", format: "esm", packages: "external", jsx: "automatic" });
      run(process.execPath, ["--input-type=module", "-e", `
        import assert from 'node:assert/strict';
        import React from 'react';
        import { renderToString } from 'react-dom/server';
        globalThis.fetch = () => { throw new Error('network during SSR'); };
        for (const key of ['indexedDB', 'Worker', 'window', 'document']) {
          Object.defineProperty(globalThis, key, { configurable: true, get() { throw new Error(key + ' during SSR'); } });
        }
        const { WalletPanel } = await import('./ssr.mjs');
        assert.ok(renderToString(React.createElement(WalletPanel)).includes('Opening wallet'));
        console.log('React SSR: no storage, engine or network work');
      `], consumer);
    }
    if (example === "next-wallet") {
      const { verifyNextWallet } = await import("./test-next-wallet.mjs");
      await verifyNextWallet(consumer, playwright, playwright ? browserNames : []);
    } else if (playwright) {
      const { verifyExampleBrowser } = await import("./test-example-browser.mjs");
      if (example === "remote-backup") {
        const { verifyRemoteBackupBrowser } = await import("./test-remote-backup-browser.mjs");
        if (browserNames.includes("chromium")) await verifyRemoteBackupBrowser(consumer, playwright);
        else console.log("Remote WebAuthn verifier runs in the Chromium shard (virtual authenticator requires CDP)");
      } else for (const browserName of browserNames) await verifyExampleBrowser(consumer, playwright, example, browserName);
    }
  }
  console.log("External SDK consumer passed: installed archives, types, ESM, real WASM, Vite workers");
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
