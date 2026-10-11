#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { reuseConsumerArchives } from "./consumer-archives.mjs";
import { archiveMembers, createExample, inspectArchives } from "./create-example.mjs";
import { buildConsumer } from "./build-consumer.mjs";
import { launchBrowser } from "./browser-launch.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scratch = mkdtempSync(join(tmpdir(), "z-stack-gift-consumer-"));
const archives = join(scratch, "archives");
const pure = join(scratch, "pure");
const app = join(scratch, "browser");
const names = ["core", "passkey", "sdk", "gift-cards"];
function run(command, args, cwd = root) {
  const result = spawnSync(command, args, { cwd, stdio: "inherit", shell: process.platform === "win32",
    env: { ...process.env, NODE_OPTIONS: "" } });
  assert.equal(result.status, 0, `${command} failed`);
}
let server;
try {
  if (!reuseConsumerArchives(root, archives, names)) {
    run(process.execPath, ["scripts/pack-sdk.mjs", archives]);
    run(process.execPath, ["scripts/pack-sdk.mjs", archives, "--gift-cards-only"]);
  }
  const packages = inspectArchives(archives, undefined, names);
  const gift = packages.get("@z-stack/gift-cards");
  const members = archiveMembers(gift.file);
  const manifest = JSON.parse(members.get("package/package.json"));
  assert.deepEqual(Object.keys(manifest.dependencies ?? {}), []);
  assert.equal(manifest.peerDependencies["@z-stack/sdk"], gift.version);
  assert.equal(manifest.peerDependenciesMeta["@z-stack/sdk"].optional, true);
  assert.deepEqual(Object.keys(manifest.exports).sort(), [".", "./browser"]);
  for (const entry of Object.values(manifest.exports)) assert.ok(!("@z-stack/source" in entry));
  assert.ok(members.has("package/dist/gift.worker.js"), "archive must contain the emitted isolated worker");
  assert.ok(![...members.keys()].some(path => path.startsWith("package/src/") || path.startsWith("package/node_modules/")),
    "optional package must not include source internals or bundled SDK dependencies");

  mkdirSync(join(pure, "vendor"), { recursive: true });
  cpSync(gift.file, join(pure, "vendor", gift.filename));
  writeFileSync(join(pure, "package.json"), JSON.stringify({ name: "gift-pure-consumer", private: true,
    type: "module", dependencies: { "@z-stack/gift-cards": `file:vendor/${gift.filename}` } }, null, 2));
  run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund"], pure);
  for (const name of ["sdk", "core", "passkey", "base"]) assert.ok(!existsSync(join(pure, "node_modules/@z-stack", name)),
    `pure gift-card installation must not pull in ${name}`);
  writeFileSync(join(pure, "consumer.mjs"), `
import assert from "node:assert/strict";
for (const key of ["window", "document", "navigator", "Worker", "indexedDB"]) Object.defineProperty(globalThis, key,
  { configurable: true, get() { throw new Error("Browser initialization during pure root import: " + key); } });
const { encodeGiftLink, parseGiftLink, giftFundingAmount, createGiftCards, GIFT_CLAIM_RESERVE_ZAT } = await import("@z-stack/gift-cards");
const card = { network: "regtest", entropy: new Uint8Array(32), birthdayHeight: 1, amountZat: 100000n };
const endpoint = "https://wallet.example/gift";
const encoded = encodeGiftLink(card, endpoint);
const decoded = parseGiftLink(encoded, { network: "regtest", endpoints: [endpoint] });
assert.deepEqual(decoded.entropy, card.entropy);
assert.equal(decoded.amountZat, card.amountZat);
assert.equal(giftFundingAmount(decoded), card.amountZat + GIFT_CLAIM_RESERVE_ZAT);
assert.equal(typeof createGiftCards, "function");
assert.throws(() => parseGiftLink(encoded, { network: "testnet", endpoints: [endpoint] }), error => error.code === "wrong_network");
assert.throws(() => parseGiftLink(encoded.replace("/gift#", "/unknown#"), { network: "regtest", endpoints: [endpoint] }), error => !error.message.includes(encoded));
`);
  run(process.execPath, [join(pure, "consumer.mjs")], pure);
  writeFileSync(join(pure, "consumer.ts"), `import { giftFundingAmount, type GiftCard, type GiftBackend, type GiftStore } from "@z-stack/gift-cards";
const card: GiftCard = { network: "regtest", entropy: new Uint8Array(32), birthdayHeight: 1, amountZat: 100000n };
const funding: bigint = giftFundingAmount(card);
export type NativeIntegration = { backend: GiftBackend; encryptedStore: GiftStore };
export { funding };\n`);
  const tsc = createRequire(join(root, "package.json")).resolve("typescript/bin/tsc");
  run(process.execPath, [tsc, "--strict", "--noEmit", "--module", "NodeNext", "--moduleResolution", "NodeNext",
    "--target", "ES2022", "--lib", "ES2022,DOM", "consumer.ts"], pure);

  createExample("browser-wallet", app, archives, { withGiftCards: true });
  const appManifest = JSON.parse(readFileSync(join(app, "package.json")));
  assert.ok(appManifest.dependencies["@z-stack/gift-cards"].startsWith("file:vendor/"));
  assert.ok(!appManifest.dependencies["@z-stack/base"]);
  run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund"], app);
  writeFileSync(join(app, "vite.config.ts"), `import { defineConfig } from "vite";
import { zStack } from "@z-stack/sdk/vite";
export default defineConfig({ plugins: [zStack()], optimizeDeps: { exclude: ["@z-stack/gift-cards"] }, build: { target: "es2022" } });\n`);
  writeFileSync(join(app, "index.html"), '<!doctype html><html><head><meta charset="UTF-8"></head><body><script type="module" src="/src/gift-fixture.ts"></script></body></html>\n');
  writeFileSync(join(app, "src/gift-fixture.ts"), `
import { createWallet, memoryWalletStorage } from "@z-stack/sdk";
import { deriveGiftAccount } from "@z-stack/sdk/gift-keys";
import { createBrowserGiftBackend } from "@z-stack/gift-cards/browser";
import type { GiftCard } from "@z-stack/gift-cards";
const check = (value: unknown, message: string) => { if (!value) throw new Error(message); };
async function smoke() {
  let stage = "main-wallet startup";
  const options = { network: "regtest" as const, server: location.origin + "/light", regtestNu63Height: 150,
    regtestNu7Height: 250, threads: 1, preferMulticore: false, autoShield: false, transparent: false, prewarmProvingKey: false };
  const main = await createWallet({ ...options, storage: memoryWalletStorage() });
  const backend = createBrowserGiftBackend({ network: options.network, server: options.server,
    namespace: "archive-gift-smoke", regtestNu63Height: options.regtestNu63Height, regtestNu7Height: options.regtestNu7Height });
  try {
    const created = await main.create({ birthday: 1 });
    main.lock();
    const before = await main.getWallet();
    stage = "gift account generation";
    const fresh = await backend.create("regtest");
    check(fresh.entropy.length === 32 && /^[0-9a-f]{64}$/.test(fresh.fingerprint), "Gift worker must generate real Rust account keys");
    const card: GiftCard = { network: "regtest", entropy: new Uint8Array(32), birthdayHeight: 1, amountZat: 100000n };
    stage = "gift account derivation";
    const derived = await backend.derive(card);
    const expected = await deriveGiftAccount(card.entropy, card.network);
    check(derived.address === expected.address && derived.fingerprint === expected.fingerprint, "Worker and public Rust derivation must agree");
    stage = "gift wallet open";
    let isolated = await backend.open(card);
    const snapshot = await isolated.getWallet();
    check(snapshot.network === "regtest" && snapshot.unifiedAddress !== before.unifiedAddress, "Gift wallet must stay independent");
    stage = "gift wallet duplicate lease";
    let conflict = false;
    try { const duplicate = await backend.open(card); await duplicate.close(); }
    catch (error) { conflict = error instanceof Error && "code" in error && error.code === "conflict"; }
    check(conflict, "A second gift worker must not acquire an active account lease");
    const after = await main.getWallet();
    check(after.unifiedAddress === created.wallet.unifiedAddress && after.ufvk === before.ufvk && !main.hasSpendingSeed(), "Gift restore must preserve the locked main wallet");
    await isolated.close();
    stage = "gift wallet durable reopen";
    isolated = await backend.open(card);
    check((await isolated.getWallet()).unifiedAddress === snapshot.unifiedAddress, "Gift wallet must reopen its durable snapshot");
    await isolated.close();
    check((await main.getWallet()).unifiedAddress === before.unifiedAddress, "Gift close must preserve the main wallet");
    return { generated: true, isolated: true, reopened: true, leaseProtected: true, preservedMainWallet: true };
  } catch (error) { throw new Error(stage + ": " + (error instanceof Error ? error.message : "gift worker failure")); }
  finally { await backend.dispose(); await main.close(); }
}
Object.assign(globalThis, { runGiftArchiveSmoke: smoke });
`);
  const guide = readFileSync(join(root, "docs/GIFT-CARDS.md"), "utf8");
  const snippets = [...guide.matchAll(/```ts\n([\s\S]*?)\n```/g)].map(match => match[1]);
  assert.ok(snippets.length >= 2, "gift guide must provide installed-package examples");
  for (const [index, snippet] of snippets.entries()) writeFileSync(join(app, "src", `gift-guide-${index}.ts`), snippet);
  buildConsumer(app, { env: { NODE_OPTIONS: "" } });
  const assets = readdirSync(join(app, "dist/assets"));
  assert.ok(assets.some(name => /^gift\.worker-.+\.js$/.test(name)), "Vite must emit the packaged gift worker");

  if (process.argv.includes("--browser")) {
    let lightRequests = 0;
    server = createServer((request, response) => {
      const pathname = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
      if (pathname.startsWith("/light")) {
        lightRequests++;
        if (pathname.endsWith("/GetLatestBlock") && request.method === "POST") {
          response.writeHead(200, { "Content-Type": "application/grpc-web+proto", "grpc-status": "0" });
          // A public tip of one, encoded as a real gRPC-Web BlockID response.
          response.end(Buffer.from([0, 0, 0, 0, 2, 8, 1]));
        } else { response.writeHead(503); response.end(); }
        return;
      }
      const file = resolve(app, "dist", "." + (pathname === "/" ? "/index.html" : pathname));
      if (!file.startsWith(resolve(app, "dist") + "/") || !existsSync(file)) { response.writeHead(404); response.end(); return; }
      response.writeHead(200, { "Content-Type": file.endsWith(".wasm") ? "application/wasm" : file.endsWith(".js") ? "text/javascript" : file.endsWith(".json") ? "application/json" : "text/html",
        "Cross-Origin-Opener-Policy": "same-origin", "Cross-Origin-Embedder-Policy": "credentialless" });
      response.end(readFileSync(file));
    });
    await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
    const origin = `http://127.0.0.1:${server.address().port}`;
    const playwright = createRequire(join(root, "package.json"))("playwright");
    const browsers = (process.argv.find(arg => arg.startsWith("--browsers="))?.slice(11) ?? "chromium").split(",");
    for (const name of browsers) {
      assert.ok(["chromium", "firefox", "webkit"].includes(name), "unknown browser");
      const browser = await launchBrowser(playwright[name]);
      try {
        const before = lightRequests;
        const page = await browser.newPage();
        const errors = [];
        page.on("pageerror", error => errors.push(error.message));
        await page.goto(origin);
        await page.waitForFunction(() => typeof globalThis.runGiftArchiveSmoke === "function");
        const result = await page.evaluate(() => globalThis.runGiftArchiveSmoke());
        assert.deepEqual(result, { generated: true, isolated: true, reopened: true, leaseProtected: true, preservedMainWallet: true });
        assert.equal(lightRequests, before + 1, "explicit initial gift open checks only the public tip; durable reopen needs no restore");
        assert.deepEqual(errors, []);
        console.log(`${name}: installed gift worker, real Rust keys, durable reopen and main wallet preservation passed`);
      } finally { await browser.close(); }
    }
  }
  console.log("Optional gift-card archive imports without the SDK and builds its isolated worker with the matching SDK");
} finally {
  if (server) await new Promise(resolve => server.close(resolve));
  rmSync(scratch, { recursive: true, force: true });
}
