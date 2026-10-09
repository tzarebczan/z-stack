import { launchBrowser } from "./browser-launch.mjs";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve, sep } from "node:path";

// This fixture is built only in the temporary consumer, never in example archives.
const fixture = `
import { createWallet, deriveAccount, type BlockTransport } from '@z-stack/sdk';
import { checkWalletSetup } from '@z-stack/sdk/diagnostics';
import { checkWalletStorageAdapter, checkVaultStoreAdapter } from '@z-stack/sdk/lab';
import { indexedDbWalletStorage } from '@z-stack/sdk';
import { indexedDbVaultStore } from '@z-stack/sdk/services';

const setup = await checkWalletSetup({ assets: true, worker: true, timeoutMs: 60000 });
if (!setup.ok) throw new Error('setup diagnostic failed: ' + JSON.stringify(setup));
const storageReport = await checkWalletStorageAdapter({ open: () => indexedDbWalletStorage({ name: 'package-conformance-wallet' }), dispose() {} });
const vaultReport = await checkVaultStoreAdapter({ open: () => indexedDbVaultStore({ dbName: 'package-conformance-vault' }), dispose() {} });
if (!storageReport.passed.length || !vaultReport.passed.length) throw new Error('adapter checks did not run');

function block(height: number) {
  // Empty regtest compact, with contiguous hashes and zero commitment counts.
  const metadata = [8, 0, 16, 0, 24, 0];
  const proto = new Uint8Array([16, height, 26, 32, ...new Array(32).fill(height),
    34, 32, ...new Array(32).fill(height - 1), 66, metadata.length, ...metadata]);
  const framed = new Uint8Array(proto.length + 4);
  new DataView(framed.buffer).setUint32(0, proto.length);
  framed.set(proto, 4);
  return framed;
}
let tip = 1;
const transport: BlockTransport = {
  kind: 'package-test', label: 'Offline regtest fixture', tip: async () => tip,
  blocks: async (start, end) => {
    if (start !== end) throw new Error('fixture expects one block');
    return block(start);
  },
};
const wallet = await createWallet({ network: 'regtest', server: transport,
  autoShield: false, prewarmProvingKey: false, memoFetch: 'on-demand', threads: 2 });
const events: string[] = [];
const off = wallet.on('sync', progress => events.push(progress.stage));
const expected = localStorage.getItem('package-test-address');
let current = await wallet.load();
if (expected) {
  if (current?.unifiedAddress !== expected || current.scannedHeight !== 2) throw new Error('saved wallet did not reload');
} else {
  if (current) throw new Error('fresh origin unexpectedly contains a wallet');
  const created = await wallet.create({ birthday: 1 });
  if (!created.recoveryPhrase || created.recoveryPhrase.split(' ').length !== 24) throw new Error('create omitted phrase');
  if ((await wallet.getWallet()).mnemonic) throw new Error('phrase returned more than once');
  const account = deriveAccount(created.recoveryPhrase, 'regtest');
  await wallet.setUnlockPolicy('each-spend');
  await wallet.restoreUfvk(account.ufvk, { birthday: 1, replace: true });
  if (wallet.hasSpendingSeed()) throw new Error('view-only restore retained spending seed');
  await wallet.sync();
  if ((await wallet.getWallet()).scannedHeight !== 1) throw new Error('initial sync failed');
  tip = 2;
  current = await wallet.sync();
  if (current.scannedHeight !== 2) throw new Error('incremental sync failed');
  if (!events.includes('synced')) throw new Error('sync event missing');
  if ((await wallet.history()).length !== 0) throw new Error('empty chain invented history');
  localStorage.setItem('package-test-address', current.unifiedAddress);
}
// Wait for scanner hydration so runtime mode is final before recording it.
await wallet.getWallet();
const checked = { mode: wallet.runtime.mode, threads: wallet.runtime.threads,
  scanned: current!.scannedHeight, loaded: !!expected };
off();
await wallet.close();
// Same realm, saved wallet: cached page bindings must restart the retired scanner.
const reopened = await createWallet({ network: 'regtest', server: transport,
  autoShield: false, prewarmProvingKey: false, memoFetch: 'on-demand', threads: 2 });
const savedAgain = await reopened.load();
if (savedAgain?.unifiedAddress !== current!.unifiedAddress) throw new Error('new owner lost saved wallet');
if ((await reopened.history()).length !== 0) throw new Error('reopened empty chain invented history');
if (reopened.hasSpendingSeed()) throw new Error('new owner retained spending seed');
const reopenedMode = reopened.runtime.mode;
// Exercise optional pending-aware removal in every browser/runtime, then recover identity.
await reopened.forget({ pending: 'reject' });
if (await reopened.load()) throw new Error('guarded empty-wallet deletion retained a saved wallet');
await reopened.restoreUfvk(savedAgain!.ufvk!, { birthday: 1 });
tip = 1; await reopened.sync(); tip = 2; await reopened.sync();
if ((await reopened.getWallet()).unifiedAddress !== current!.unifiedAddress) throw new Error('guarded deletion/recovery changed identity');
await reopened.close();
Object.assign(window, { packageCheck: { ...checked, reopened: reopenedMode === checked.mode } });
Object.assign(window, { packageReplace: async () => {
  const replacing = await createWallet({ network: 'regtest', server: transport,
    autoShield: false, prewarmProvingKey: false, memoFetch: 'on-demand', threads: 2 });
  const saved = await replacing.load();
  if (!saved?.ufvk) throw new Error('replacement fixture has no saved viewing key');
  await replacing.restoreUfvk(saved.ufvk, { birthday: 1, replace: true, beforeCommit: async () => {
    Object.assign(window, { packageReplacementReady: true });
    await new Promise<void>(() => {}); // A discarded page never runs rollback/finally.
  } });
} });
`;

export async function verifyBrowserPackages(app, browsers, browserName = "chromium") {
  const chromium = browsers[browserName];
  assert.ok(chromium, "unsupported browser engine");
  const originalIndex = readFileSync(join(app, "index.html"));
  await verifyExampleRecovery(app, chromium);
  writeFileSync(join(app, "src", "package-check.ts"), fixture);
  writeFileSync(join(app, "index.html"), '<!doctype html><html><head><link rel="icon" href="data:,"></head><body><script type="module" src="/src/package-check.ts"></script></body></html>');
  const built = spawnSync("npm", ["run", "build"], { cwd: app, stdio: "inherit", shell: process.platform === "win32" });
  assert.equal(built.status, 0, "browser fixture failed to typecheck/build");
  const browser = await launchBrowser(chromium, { headless: true });
  try {
    for (const isolated of [false, true]) {
      const dist = resolve(app, "dist");
      const server = createServer((req, res) => {
        if (isolated) {
          res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
          res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
        }
        const path = resolve(dist, `.${new URL(req.url, "http://localhost").pathname === "/" ? "/index.html" : new URL(req.url, "http://localhost").pathname}`);
        if (!path.startsWith(dist + sep)) { res.writeHead(403).end(); return; }
        try {
          res.setHeader("Content-Type", path.endsWith(".wasm") ? "application/wasm" : path.endsWith(".js") ? "application/javascript" : "text/html");
          res.end(readFileSync(path));
        } catch { res.writeHead(404).end(); }
      });
      await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
      const context = await browser.newContext();
      try {
        let page = await context.newPage();
        const errors = [];
        const workerUrls = [];
        const recordConsole = message => { if (message.type() === "error" || message.type() === "warning") errors.push(message.text()); };
        page.on("pageerror", error => errors.push(error.message));
        page.on("console", recordConsole);
        page.on("worker", worker => workerUrls.push(worker.url()));
        await context.route("**/*", route => {
          assert.equal(new URL(route.request().url()).hostname, "127.0.0.1", "fixture contacted a remote host");
          return route.continue();
        });
        await page.goto(`http://127.0.0.1:${server.address().port}`);
        await page.waitForFunction(() => !!window.packageCheck, null, { timeout: 90_000 });
        const result = await page.evaluate(() => window.packageCheck);
        assert.equal(result.mode, isolated ? "multi-thread" : "single-thread");
        assert.equal(result.threads, isolated ? 2 : 1);
        assert.equal(result.scanned, 2);
        assert.equal(result.loaded, false);
        assert.equal(result.reopened, true);
        for (const name of ["setup.worker", "scan.worker", "prove.worker"]) assert.ok(workerUrls.some(url => url.includes(name)), `${name} did not start`);
        await page.reload();
        await page.waitForFunction(() => !!window.packageCheck, null, { timeout: 90_000 });
        assert.equal((await page.evaluate(() => window.packageCheck)).loaded, true);
        await page.evaluate(() => { void window.packageReplace(); });
        await page.waitForFunction(() => window.packageReplacementReady === true, null, { timeout: 90_000 });
        await page.close(); // Stop a live replacement after derivation, before its snapshot commit.
        page = await context.newPage();
        page.on("pageerror", error => errors.push(error.message));
        page.on("console", recordConsole);
        await page.goto(`http://127.0.0.1:${server.address().port}`);
        await page.waitForFunction(() => !!window.packageCheck, null, { timeout: 90_000 });
        assert.equal((await page.evaluate(() => window.packageCheck)).loaded, true);
        assert.deepEqual(errors, [], "browser reported runtime errors or fallback warnings");
        console.log(`Installed ${browserName} SDK: ${result.mode}, ${result.threads} threads, create/restore/sync/reload/discarded-replacement passed`);
      } finally {
        await context.close();
        await new Promise(resolve => server.close(resolve));
      }
    }
  } finally {
    await browser.close();
    writeFileSync(join(app, "index.html"), originalIndex);
  }
}

export async function verifyExampleRecovery(app, chromium) {
  const mainPath = join(app, "src", "main.ts");
  const main = readFileSync(mainPath, "utf8");
  // Preserve the example's UI code. Only its SDK factory is wrapped in this
  // temporary consumer to inject a history-read failure after real persistence.
  writeFileSync(mainPath, main.replace('from "@z-stack/sdk";', 'from "./recovery-fixture";'));
  writeFileSync(join(app, "src", "recovery-fixture.ts"), `
    import { createWallet as realCreateWallet, type BlockTransport } from '@z-stack/sdk';
    export * from '@z-stack/sdk';
    declare global { interface Window { fixtureTipRequests?: number; fixtureTipUnavailable?: boolean; fixtureForgetWallet?: () => Promise<void>; } }
    export async function createWallet(options: Parameters<typeof realCreateWallet>[0]) {
      const transport: BlockTransport = {
        kind: 'recovery-test', label: 'Offline recovery fixture', tip: async () => {
          Object.assign(window, {fixtureTipRequests: (window.fixtureTipRequests ?? 0) + 1});
          if (window.fixtureTipUnavailable) throw new Error('grpc/transport SYNTHETIC_PRIVATE_PROVIDER_CONTEXT');
          return 1;
        },
        blocks: async (start, end) => {
          if (start !== 1 || end !== 1) throw new Error('unexpected fixture range');
          const metadata = [8,0,16,0,24,0];
          const proto = [16,1,26,32,...new Array(32).fill(1),34,32,...new Array(32).fill(0),66,metadata.length,...metadata];
          return new Uint8Array([0,0,0,proto.length,...proto]);
        },
      };
      const wallet = await realCreateWallet({ ...options, network: "regtest", server: transport, prewarmProvingKey: false });
      Object.assign(window, {fixtureForgetWallet: () => wallet.forget({passkey:true})});
      let failHistory = true;
      return new Proxy(wallet, {
        get(target, key) {
          if (key === 'history') return async (...args: Parameters<typeof target.history>) => { if (failHistory) { failHistory=false; throw new Error('injected activity read failure'); } return target.history(...args); };
          const value = Reflect.get(target, key, target);
          return typeof value === 'function' ? value.bind(target) : value;
        },
      });
    }
  `);
  const built = spawnSync("npm", ["run", "build"], { cwd: app, stdio: "inherit", shell: process.platform === "win32" });
  assert.equal(built.status, 0, "example recovery fixture failed to build");
  const dist = resolve(app, "dist");
  const server = createServer((req, res) => {
    const pathname = new URL(req.url, "http://localhost").pathname;
    const path = resolve(dist, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!path.startsWith(dist + sep)) { res.writeHead(403).end(); return; }
    try {
      res.setHeader("Content-Type", path.endsWith(".wasm") ? "application/wasm" : path.endsWith(".js") ? "application/javascript" : path.endsWith(".css") ? "text/css" : "text/html");
      res.end(readFileSync(path));
    } catch { res.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  let browser;
  try {
    browser = await launchBrowser(chromium, { headless: true });
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [], diagnostics = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => diagnostics.push(message.text()));
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "clipboard", { configurable:true, value:{writeText:async value => {
        if (window.rejectCopy) throw new Error("clipboard denied");
        window.copiedValue = value;
      }} });
    });
    await page.route("**/*", route => {
      assert.equal(new URL(route.request().url()).hostname, "127.0.0.1", "recovery fixture contacted a remote host");
      return route.continue();
    });
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForFunction(() => document.getElementById("status").textContent === "Create a wallet or restore one.");
    assert.equal(await page.locator("#sync").isDisabled(), true);
    assert.equal(await page.locator("#lock").isDisabled(), true);
    await page.locator("#words").fill("not a valid recovery phrase");
    await page.locator("#birthday").fill("1");
    await page.locator("#restore").click();
    await page.locator("#status").filter({hasText:"Those words are not a valid recovery phrase."}).waitFor();
    assert.equal(await page.locator("#words").inputValue(), "not a valid recovery phrase", "invalid restore erased its input");
    await page.locator("#clear-words").click();
    await page.locator("#create").click();
    await page.locator("#phrase").waitFor({state:"visible"});
    assert.equal(await page.locator("#restore-form").isVisible(), false);
    assert.equal(await page.locator("#copy-phrase").isEnabled(), true);
    const phrase = await page.locator("#phrase").textContent();
    assert.equal(phrase.trim().split(/\s+/).length, 24);
    await page.locator("#copy-phrase").click();
    assert.equal(await page.evaluate(() => window.copiedValue), phrase.trim());
    await page.locator("#phrase-copy-status").filter({hasText:"clipboard"}).waitFor();
    await page.evaluate(() => { window.rejectCopy = true; });
    await page.locator("#copy-phrase").click();
    await page.locator("#phrase-copy-status").filter({hasText:"Save the numbered words in order"}).waitFor();
    await page.evaluate(() => { window.rejectCopy = false; });
    assert.equal(await page.locator("#create").isDisabled(), true);
    assert.equal(await page.locator("#clear-words").isDisabled(), true);
    page.once("dialog", dialog => dialog.accept());
    await page.reload();
    await page.waitForFunction(() => document.getElementById("status").textContent === "Create a wallet or restore one.");
    assert.equal(await page.locator("#address").textContent(), "", "unconfirmed creation persisted on reload");
    await page.locator("#create").click();
    await page.locator("#phrase").waitFor({state:"visible"});
    const confirmedPhrase = (await page.locator("#phrase").textContent()).trim();
    assert.equal(confirmedPhrase.split(/\s+/).length, 24);
    await page.locator("#hide-phrase").click();
    await page.waitForFunction(() => !document.getElementById("sync").disabled);
    const originalAddress = await page.locator("#address").textContent();
    assert.ok(originalAddress, "confirmed wallet creation did not complete");
    assert.equal(await page.locator("#phrase").textContent(), "");
    assert.equal(await page.locator("#phrase").isVisible(), false);
    assert.equal(await page.locator("#review-send").isDisabled(), true);
    assert.equal(await page.locator("#copy-address").isVisible(), true);
    await page.locator("#copy-address").click();
    assert.equal(await page.evaluate(() => window.copiedValue), await page.locator("#address").textContent());
    await page.evaluate(() => { window.rejectCopy = true; });
    await page.locator("#copy-address").click();
    await page.locator("#copy-status").filter({hasText:"Could not copy"}).waitFor();
    assert.equal(await page.locator("#create").isDisabled(), true);
    assert.equal(await page.locator("#restore-form").isVisible(), false, "restore stays hidden with a saved wallet");
    assert.equal(await page.locator("#create-panel").isVisible(), false);
    assert.equal(await page.locator("#remove-panel").isVisible(), true);
    // The defensive refusal still protects programmatic or stale submissions.
    await page.evaluate(phrase => {
      document.querySelector("#words").value = phrase;
      document.querySelector("#birthday").value = "1";
      document.querySelector("#restore-form").dispatchEvent(new Event("submit", {bubbles:true,cancelable:true}));
    }, phrase.trim());
    await page.waitForFunction(() => document.getElementById("status").textContent.includes("already saved"));
    assert.equal(await page.locator("#words").inputValue(), phrase.trim(), "refused restore erased the phrase");
    assert.equal(await page.locator("#birthday").inputValue(), "1");
    await page.locator("#sync").click();
    await page.waitForFunction(() => !document.getElementById("review-send").disabled, null, {timeout:90000});
    await page.locator("#send-to").fill(await page.locator("#address").textContent());
    await page.locator("#send-amount").fill("0.1");
    await page.locator("#review-send").click();
    await page.locator("#send-status").filter({hasText:"Not enough shielded funds"}).waitFor();
    await page.locator("#send-amount").fill("invalid");
    await page.locator("#review-send").click();
    await page.locator("#send-status").filter({hasText:"Enter a valid amount"}).waitFor();
    assert.ok(!(await page.locator("#send-status").textContent()).includes("ZEC"));
    await page.locator("#send-to").fill("not-an-address");
    await page.locator("#send-amount").fill("0.01");
    await page.locator("#review-send").click();
    await page.locator("#send-status").filter({hasText:"not a valid Zcash destination"}).waitFor();
    assert.equal(await page.locator("#status").textContent(), "");
    await page.locator("#send-to").fill("zcash:fixture");
    await page.locator("#send-amount").fill("0.1");
    await page.locator("#review-send").click();
    await page.locator("#send-status").filter({hasText:"This form does not accept zcash:"}).waitFor();
    await page.locator("#send-to").fill("");
    await page.locator("#review-send").click();
    assert.equal(await page.locator("#status").textContent(), "");
    await page.locator("#send-status").filter({hasText:"Complete the required payment fields."}).waitFor();
    await page.reload();
    await page.waitForFunction(() => !document.getElementById("sync").disabled);
    assert.ok(await page.locator("#address").textContent(), "the created wallet did not persist through reload");
    await page.setViewportSize({width:390,height:844});
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, "wallet example overflows mobile viewport");
    // Local removal is explicit, clears all stale form/receipt state and survives reload.
    await page.locator("#remove-panel summary").click();
    assert.equal(await page.locator("#remove").isDisabled(), true);
    await page.locator("#remove-confirm").check();
    await page.locator("#remove").click();
    await page.locator("#status").filter({hasText:"Local wallet removed."}).waitFor();
    assert.equal(await page.locator("#words").inputValue(), "");
    assert.equal(await page.locator("#birthday").inputValue(), "");
    assert.equal(await page.locator("#send-to").inputValue(), "");
    assert.equal(await page.locator("#address").textContent(), "");
    assert.equal(await page.locator("#restore-form").isVisible(), true);
    assert.equal(await page.locator("#send-panel").isVisible(), false);
    await page.reload();
    await page.locator("#status").filter({hasText:"Create a wallet or restore one."}).waitFor();
    // Automatic creation needs a server; numeric creation does not.
    await page.evaluate(() => { window.fixtureTipUnavailable = true; window.fixtureTipRequests = 0; });
    await page.locator("#create").click();
    await page.locator("#status").filter({hasText:"Automatic birthday needs the light server"}).waitFor();
    assert.equal(await page.locator("#phrase").isVisible(), false);
    assert.equal(await page.evaluate(() => window.fixtureTipRequests), 1);
    await page.locator("#create-birthday").fill("0"); await page.locator("#create").click();
    assert.equal(await page.locator("#create-birthday").evaluate(input => input.checkValidity()), false);
    await page.locator("#create-birthday").fill("1");
    await page.locator("#create").click(); await page.locator("#phrase").waitFor({state:"visible"});
    assert.equal(await page.evaluate(() => window.fixtureTipRequests), 1, "explicit height fetched the tip");
    await page.locator("#hide-phrase").click(); await page.waitForFunction(() => !document.getElementById("sync").disabled);
    await page.reload(); await page.waitForFunction(() => !document.getElementById("sync").disabled);
    assert.equal(await page.locator("#restore-form").isVisible(), false);
    await page.locator("#remove-panel summary").click(); await page.locator("#remove-confirm").check(); await page.locator("#remove").click();
    await page.locator("#status").filter({hasText:"Local wallet removed."}).waitFor();
    await page.locator("#words").fill(confirmedPhrase); await page.locator("#birthday").fill("1"); await page.locator("#restore").click();
    await page.locator("#status").filter({hasText:"Wallet restored."}).waitFor();
    assert.equal(await page.locator("#words").inputValue(), "");
    assert.equal(await page.locator("#restore-form").isVisible(), false);
    await page.reload(); await page.waitForFunction(() => !document.getElementById("sync").disabled);
    assert.equal(await page.locator("#address").textContent(), originalAddress, "restore recovered a different identity");
    assert.ok(diagnostics.every(message => !message.includes("SYNTHETIC_PRIVATE_PROVIDER_CONTEXT")), "provider context leaked");
    assert.ok(diagnostics.every(message => !message.includes("injected activity read failure")), "raw provider error reached the console");
    // A competing realm removes the wallet after this page rendered it.
    const other = await page.context().newPage();
    try {
      await other.goto(page.url());
      await other.waitForFunction(() => !document.getElementById("sync").disabled);
      await other.evaluate(() => window.fixtureForgetWallet());
      await page.locator("#remove-panel summary").click();
      await page.locator("#remove-confirm").check(); await page.locator("#remove").click();
      await page.locator("#status").filter({hasText:"Local wallet removed."}).waitFor();
      assert.equal(await page.locator("#address").textContent(), "", "competing deletion left a stale receive address");
      assert.equal(await page.locator("#send-panel").isVisible(), false);
      assert.equal(await page.locator("#restore-form").isVisible(), true);
      assert.equal(await page.locator("#history li").count(), 0);
      assert.equal(await page.locator("#sync").isDisabled(), true);
    } finally { await other.close(); }
    assert.deepEqual(errors, [], "example recovery failure escaped its UI handler");
    console.log("Installed example: confirmation precedes persistence; unconfirmed reload cancels; confirmed wallet survives activity-read failure/retry/reload");
  } finally {
    if (browser) await browser.close();
    await new Promise(resolve => server.close(resolve));
    writeFileSync(mainPath, main);
  }
}
