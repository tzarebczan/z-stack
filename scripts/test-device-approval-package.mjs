#!/usr/bin/env node
// Run the same real-WebCrypto contract against installed distribution bytes in Node and browsers.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { reuseConsumerArchives } from "./consumer-archives.mjs";
import { launchBrowser } from "./browser-launch.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const scratch = mkdtempSync(join(tmpdir(), "z-stack-device-approval-"));
const archives = join(scratch, "archives"), app = join(scratch, "consumer");
const run = (command, args, cwd = app) => {
  const result = spawnSync(command, args, { cwd, stdio: "inherit", env: { ...process.env, NODE_OPTIONS: "" } });
  assert.equal(result.status, 0, `${command} failed`);
};
let server;
try {
  mkdirSync(archives); mkdirSync(app);
  if (!reuseConsumerArchives(root, archives, ["passkey"]))
    run("pnpm", ["pack", "--pack-destination", archives], join(root, "packages/passkey"));
  const version = JSON.parse(readFileSync(join(root, "package.json"))).version;
  writeFileSync(join(app, "package.json"), JSON.stringify({ private: true, type: "module" }));
  run("npm", ["install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--cache", join(scratch, "empty-cache"), "--registry", "http://127.0.0.1:1", join(archives, `z-stack-passkey-${version}.tgz`)]);
  const installed = join(app, "node_modules/@z-stack/passkey");
  const manifest = JSON.parse(readFileSync(join(installed, "package.json")));
  assert.deepEqual(manifest.dependencies ?? {}, {}, "passkey must remain dependency-free");
  assert.ok(!existsSync(join(installed, "src")), "installed source aliases must not mask missing distribution exports");
  assert.ok(manifest.exports["./device-approval"]);
  assert.ok(!JSON.stringify(manifest.exports).includes("@z-stack/source"), "installed exports must use distribution bytes only");
  const scenario = readFileSync(join(root, "scripts/fixtures/device-approval-checks.mjs"), "utf8");
  writeFileSync(join(app, "checks.mjs"), scenario);
  writeFileSync(join(app, "node.mjs"), `import assert from 'node:assert/strict';import * as api from '@z-stack/passkey/device-approval';import {runDeviceApprovalChecks} from './checks.mjs';const checks=await runDeviceApprovalChecks(api);assert.ok(checks.length>=50);console.log('Installed device approval Node checks:',checks.length);`);
  run(process.execPath, ["node.mjs"]);
  writeFileSync(join(app, "browser.mjs"), `import * as api from '@z-stack/passkey/device-approval';import {runDeviceApprovalChecks} from './checks.mjs';globalThis.runDeviceApprovalChecks=()=>runDeviceApprovalChecks(api);`);
  const require = createRequire(join(root, "package.json"));
  writeFileSync(join(app, "types.ts"), `import {createDeviceApprovalRequest,sealDeviceApproval,parseDeviceApprovalRequest,parseDeviceApprovalResponse,deviceApprovalViewingKeyFingerprint,type DeviceApprovalPayload,type DeviceApprovalReceiver} from '@z-stack/passkey/device-approval';const pending:Promise<DeviceApprovalReceiver>=createDeviceApprovalRequest({origin:'https://wallet.example',mode:'view-only'});declare const payload:DeviceApprovalPayload;void sealDeviceApproval({request:'request',origin:'https://wallet.example',payload});void parseDeviceApprovalRequest({request:'request',origin:'https://wallet.example'});void parseDeviceApprovalResponse({request:'request',response:'response',origin:'https://wallet.example'});void deviceApprovalViewingKeyFingerprint({ufvk:'uview1',network:'mainnet'});void pending;`);
  run(process.execPath, [require.resolve("typescript/bin/tsc"), "--noEmit", "--strict", "--skipLibCheck", "--target", "ES2022", "--lib", "ES2022,DOM", "--module", "NodeNext", "--moduleResolution", "NodeNext", "types.ts"]);
  const toolRequire = createRequire(require.resolve("tsx/package.json"));
  const { build } = toolRequire("esbuild");
  const bundle = await build({ entryPoints: [join(app, "browser.mjs")], absWorkingDir: app, bundle: true, platform: "browser", format: "esm", write: false, metafile: true });
  assert.ok(!Object.keys(bundle.metafile.inputs).some(path => /(?:vault|webauthn|store|generated|engine)\.[cm]?[jt]s$/.test(path)), "optional approval entry must not load the vault, WebAuthn storage or wallet engine");
  const js = bundle.outputFiles[0].contents;
  if (process.argv.includes("--browser")) {
    const names = (process.argv.find(arg => arg.startsWith("--browsers="))?.slice(11) ?? "chromium,firefox,webkit").split(",");
    assert.ok(names.every(name => ["chromium", "firefox", "webkit"].includes(name)));
    server = createServer((request, response) => {
      response.writeHead(200, { "Content-Type": request.url === "/approval.js" ? "text/javascript" : "text/html", "Cache-Control": "no-store" });
      response.end(request.url === "/approval.js" ? js : '<!doctype html><script type="module" src="/approval.js"></script>');
    });
    await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
    const address = server.address(); assert.ok(address && typeof address === "object");
    const playwright = require("playwright");
    for (const name of names) {
      const browser = await launchBrowser(playwright[name]);
      try {
        const page = await browser.newPage();
        await page.goto(`http://127.0.0.1:${address.port}`);
        await page.waitForFunction(() => typeof globalThis.runDeviceApprovalChecks === "function");
        const count = await page.evaluate(async () => (await globalThis.runDeviceApprovalChecks()).length);
        assert.ok(count >= 50);
        console.log(`Installed device approval ${name} real-WebCrypto checks: ${count}`);
      } finally { await browser.close(); }
    }
  }
  console.log("Installed optional device approval entry: portable, dependency-free, engine-free browser bundle passed");
} finally {
  if (server) await new Promise(resolve => server.close(resolve));
  rmSync(scratch, { recursive: true, force: true });
}
