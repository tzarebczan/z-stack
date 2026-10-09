import { launchBrowser } from "./browser-launch.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { readFileSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";

function build(app, isolated) {
  const result = spawnSync("npm", ["run", "build"], { cwd: app, stdio: "inherit",
    shell: process.platform === "win32", env: { ...process.env, NEXT_TELEMETRY_DISABLED: "1",
      Z_STACK_ISOLATION: isolated ? "on" : "off" } });
  assert.equal(result.status, 0, "Next.js production build failed");
}
async function start(app) {
  const reservation = createServer();
  await new Promise(resolve => reservation.listen(0, "127.0.0.1", resolve));
  const port = reservation.address().port;
  await new Promise(resolve => reservation.close(resolve));
  const bin = createRequire(join(app, "package.json")).resolve("next/dist/bin/next");
  const child = spawn(process.execPath, [bin, "start", "--hostname", "127.0.0.1", "--port", String(port)],
    { cwd: app, stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, NEXT_TELEMETRY_DISABLED: "1" } });
  let output = "";
  child.stdout.on("data", bytes => { output += bytes; });
  child.stderr.on("data", bytes => { output += bytes; });
  const origin = `http://127.0.0.1:${port}`;
  const stop = async () => {
    if (child.exitCode !== null) return;
    await new Promise(resolve => {
      child.once("exit", resolve); child.kill("SIGTERM");
      const timer = setTimeout(() => child.kill("SIGKILL"), 5000); timer.unref();
      child.once("exit", () => clearTimeout(timer));
    });
  };
  try {
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      if (child.exitCode !== null) throw new Error(`Next server exited: ${output}`);
      try {
        const response = await fetch(origin, { signal: AbortSignal.timeout(2000) });
        if (response.ok) return { origin, stop };
      } catch { /* readiness only */ }
      await new Promise(resolve => setTimeout(resolve, 150));
    }
    throw new Error(`Next server did not start: ${output}`);
  } catch (error) { await stop(); throw error; }
}

// Only the app-owned chain connection changes in this temporary consumer.
// Wallet, Rust engines, storage, workers and UI are the installed production code.
const connectionFixture = `
import type { WalletOptions } from '@z-stack/sdk';
export const connection: Pick<WalletOptions, 'network' | 'server'> = {
  network: 'regtest', server: {
    kind: 'next-integration-fixture', label: 'Offline compact-block fixture',
    tip: async signal => {
      const response = await fetch('/fixture?action=tip', {signal});
      if (!response.ok) throw new Error('SYNTHETIC_PRIVATE_PROVIDER_CONTEXT');
      return (await response.json()).tip;
    },
    blocks: async (start, end, signal) => {
      const response = await fetch('/fixture?action=blocks&start='+start+'&end='+end, {signal});
      if (!response.ok) throw new Error('SYNTHETIC_PRIVATE_PROVIDER_CONTEXT');
      return new Uint8Array(await response.arrayBuffer());
    },
  },
};
`;
const routeFixture = `
export const dynamic = 'force-dynamic';
let tip = 1, fail = false, delay = false;
export async function GET(request: Request) {
  const params = new URL(request.url).searchParams;
  const action = params.get('action');
  if (action === 'configure') {
    const next = Number(params.get('tip'));
    if (![1, 2, 3].includes(next)) return new Response(null, {status:400});
    tip = next; fail = params.get('fail') === '1'; delay = params.get('delay') === '1';
    return Response.json({ok:true});
  }
  if (fail) return new Response(null, {status:503});
  if (action === 'tip') return Response.json({tip});
  if (action !== 'blocks') return new Response(null, {status:400});
  if (delay) await new Promise(resolve => setTimeout(resolve, 1500));
  const start = Number(params.get('start')), end = Number(params.get('end'));
  if (start < 1 || end > 3 || start > end) return new Response(null, {status:400});
  const frames: number[] = [];
  for(let height=start; height<=end; height++) {
    const metadata = [8,0,16,0,24,0];
    const proto = [16,height,26,32,...new Array(32).fill(height),34,32,
      ...new Array(32).fill(height-1),66,metadata.length,...metadata];
    frames.push(0,0,0,proto.length,...proto);
  }
  return new Response(new Uint8Array(frames), {headers:{'Content-Type':'application/octet-stream'}});
}
`;

export async function verifyNextWallet(app, browsers, browserNames = []) {
  // The initial build is the unmodified demo, with no test-only route or transport.
  const production = await start(app);
  try {
    const response = await fetch(production.origin);
    const html = await response.text();
    assert.ok(html.includes("Opening local wallet"), "server rendering attempted wallet initialization");
    assert.equal(response.headers.get("referrer-policy"), "no-referrer");
    assert.equal(response.headers.get("x-frame-options"), "DENY");
    assert.equal(response.headers.get("cross-origin-embedder-policy"), "require-corp");
    assert.equal((await fetch(`${production.origin}/_next/static/missing.wasm`)).status, 404);
    if (browsers) for (const name of browserNames) {
      const browser = await launchBrowser(browsers[name]);
      try {
        const page = await browser.newPage();
        page.setDefaultTimeout(30_000);
        const errors = [], external = [];
        page.on("pageerror", error => errors.push(error.message));
        await page.route("**/*", route => {
          if (new URL(route.request().url()).origin !== production.origin) {
            external.push(route.request().url()); return route.abort();
          }
          return route.continue();
        });
        await page.goto(production.origin);
        await page.getByRole("status").filter({hasText:"Ready to create or restore."}).waitFor({timeout:90_000});
        assert.deepEqual(external, [], "opening the demo contacted a chain/account service");
        assert.deepEqual(errors, [], "production startup failed");
      } finally { await browser.close(); }
    }
    console.log("Next.js production shell/headers/404: verified; wallet initialization remains client-only");
  } finally { await production.stop(); }
  if (!browsers) return;

  const path = join(app, "lib", "connection.ts");
  const original = readFileSync(path);
  const route = join(app, "app", "fixture");
  writeFileSync(path, connectionFixture); mkdirSync(route);
  writeFileSync(join(route, "route.ts"), routeFixture);
  try {
    for (const isolated of [true, false]) {
      build(app, isolated);
      const server = await start(app);
      try { for (const name of browserNames) await flows(server.origin, browsers[name], name, isolated); }
      finally { await server.stop(); }
    }
  } finally { writeFileSync(path, original); rmSync(route, {recursive:true, force:true}); }
}

async function flows(origin, engine, name, isolated) {
  const browser = await launchBrowser(engine);
  const context = await browser.newContext({ viewport: name === "chromium" ? {width:390,height:844} : {width:1280,height:900} });
  const page = await context.newPage();
  try {
    page.setDefaultTimeout(30_000);
    const errors = [], requests = [], workers = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("worker", worker => workers.push(worker.url()));
    page.on("request", request => requests.push(request.url() + (request.postData() ?? "")));
    await page.route("**/*", route => {
      assert.equal(new URL(route.request().url()).origin, origin, "demo leaked a request to a remote service");
      return route.continue();
    });
    const status = text => page.getByRole("status").filter({hasText:text});
    const configure = async options => {
      const response = await page.request.get(`${origin}/fixture?action=configure&${new URLSearchParams(options)}`);
      assert.equal(response.status(), 200);
    };
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "clipboard", {configurable:true, value:{writeText:async value => {
        if (window.rejectCopy) throw new Error("clipboard denied");
        window.copiedValue = value;
      }}});
    });
    await configure({tip:"1"});
    await page.goto(origin);
    await status("Ready to create or restore.").waitFor({timeout:90_000});
    console.log(`Next.js ${name}: browser ready (${isolated ? "isolated" : "non-isolated"})`);
    // A warm worker may already report its real mode. The key loader alone
    // must not label an isolated, MT-capable deployment as single-threaded.
    await page.getByText(isolated ? /^(Scan engine starts on sync|2 threads)$/
      : /^(Scan engine starts on sync|Single thread)$/, {exact:true}).waitFor();
    assert.equal((await page.request.get(`${origin}/favicon.svg`)).status(), 200);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, "demo overflows the viewport");
    await page.getByRole("button", {name:"Create wallet",exact:true}).click();
    await page.getByRole("heading", {name:"Save these 24 words."}).waitFor();
    const phrase = (await page.locator(".words li").allTextContents()).join(" ");
    assert.equal(phrase.split(" ").length, 24);
    const address = await page.locator("#address").textContent();
    assert.ok(address);
    assert.equal(await page.getByRole("button", {name:"Copy address",exact:true}).count(), 0);
    await page.getByRole("button", {name:"Copy recovery phrase",exact:true}).click();
    await status("Phrase copied. Your clipboard contains your recovery words.").waitFor();
    assert.equal(await page.evaluate(() => window.copiedValue), phrase);
    await page.evaluate(() => { window.rejectCopy = true; });
    await page.getByRole("button", {name:"Copy recovery phrase",exact:true}).click();
    await status("Could not copy. Save the numbered words in order.").waitFor();
    await page.evaluate(() => { window.rejectCopy = false; });
    assert.equal(await page.getByRole("button", {name:"Sync wallet",exact:true}).isDisabled(), true);
    await page.getByRole("link", {name:"How it works",exact:true}).click();
    await status("Save your recovery phrase before leaving.").waitFor();
    assert.equal(await page.locator(".words li").count(), 24, "navigation hid the one-time phrase");
    await page.getByLabel("I saved my recovery phrase").check();
    await page.getByRole("button", {name:"Done, hide phrase"}).click();
    console.log(`Next.js ${name}: create and backup validated`);
    await page.getByRole("button", {name:"Copy address",exact:true}).click();
    await status("Address copied.").waitFor();
    assert.equal(await page.evaluate(() => window.copiedValue), address);
    assert.equal(await status("Phrase copied.").count(), 0);
    assert.equal(await page.getByRole("button", {name:"Review payment",exact:true}).isDisabled(), true);
    await page.getByRole("button", {name:"Sync wallet",exact:true}).click();
    await status("Scanned through block 1.").waitFor({timeout:90_000});
    await page.getByText("Scanned to 1", {exact:true}).waitFor();
    await page.getByText(isolated ? "2 threads" : "Single thread", {exact:true}).waitFor();
    console.log(`Next.js ${name}: initial sync and runtime validated`);
    assert.equal(await page.getByRole("button", {name:"Review payment",exact:true}).isEnabled(), true);
    assert.ok(workers.length >= 1, "scanner did not run in a worker");
    await configure({tip:"2",delay:"1"});
    const balance = await page.locator("#balance").textContent();
    await page.getByRole("button", {name:"Sync wallet",exact:true}).click();
    await page.getByRole("button", {name:"Stop sync",exact:true}).waitFor();
    assert.equal(await page.locator("#balance").textContent(), balance);
    assert.equal(await page.getByText("No activity yet.", {exact:true}).isVisible(), true);
    await status("Scanned through block 2.").waitFor({timeout:90_000});
    await page.getByText("Scanned to 2", {exact:true}).waitFor();
    await configure({tip:"2",fail:"1"});
    await page.getByRole("button", {name:"Sync wallet",exact:true}).click();
    await status("Something went wrong.").waitFor();
    assert.ok(!(await page.locator("body").textContent()).includes("SYNTHETIC_PRIVATE_PROVIDER_CONTEXT"));
    await configure({tip:"2"});
    await page.reload();
    await page.waitForFunction(() => document.querySelector(".status")?.textContent !== "Opening local wallet…", null, {timeout:90_000});
    assert.match(await page.locator(".status").textContent(), /Wallet opened\. Spending is locked\./,
      `Reload failed; browser errors: ${JSON.stringify(errors)}`);
    assert.equal(await page.locator("#address").textContent(), address);
    assert.equal(await page.locator(".words").count(), 0, "reload retained recovery phrase");
    await page.getByText("Unlock with your recovery phrase", {exact:true}).click();
    await page.locator("#unlock-words").fill("not a valid recovery phrase");
    await page.getByRole("button", {name:"Unlock spending",exact:true}).click();
    await status("Those words are not a valid recovery phrase.").waitFor();
    assert.equal(await page.locator("#unlock-words").inputValue(), "", "failed unlock retained entered words");
    assert.equal(await page.locator("#address").textContent(), address, "failed unlock replaced the saved wallet");
    await page.locator("#unlock-words").fill(phrase);
    await page.getByRole("button", {name:"Unlock spending",exact:true}).click();
    assert.equal(await page.locator("#unlock-words").count() === 0 || await page.locator("#unlock-words").inputValue() === "", true);
    await status("Spending unlocked. Lock when done.").waitFor();
    await page.getByRole("button", {name:"Lock spending",exact:true}).click();
    for(let i=0;i<3;i++) {
      await page.getByRole("link", {name:"How it works",exact:true}).click();
      await page.getByRole("link", {name:"Back to wallet",exact:true}).click();
      await status("Wallet opened. Spending is locked.").waitFor({timeout:30_000});
      assert.equal(await page.locator("#address").textContent(), address);
    }
    await page.getByText("Check this deployment", {exact:true}).click();
    await page.getByRole("button", {name:"Run setup checks",exact:true}).click();
    await status("Deployment checks passed.").waitFor({timeout:90_000});
    assert.match(await page.locator(".checks li").filter({hasText:"wallet owner"}).innerText(), /info/);
    await configure({tip:"3",delay:"1"});
    await page.getByRole("button", {name:"Sync wallet",exact:true}).click();
    await page.getByRole("button", {name:"Stop sync",exact:true}).click();
    await page.waitForFunction(() => !document.querySelector(".sync-actions button").disabled);
    assert.equal(await page.locator("#balance").textContent(), balance, "cancelled sync cleared balance");
    await page.getByText("Scanned to 2", {exact:true}).waitFor();
    assert.doesNotMatch(await page.locator(".wallet-main > .status").innerText(), /Scanned through block 3/,
      "cancelled scan must not claim the target was reached");
    await configure({tip:"3"});
    await page.getByRole("button", {name:"Sync wallet",exact:true}).click();
    await status("Scanned through block 3.").waitFor({timeout:90_000});
    await page.getByText("Scanned to 3", {exact:true}).waitFor();
    assert.equal(await page.locator("#address").textContent(), address, "resumed sync replaced the wallet");
    await configure({tip:"2"});
    await page.getByText("Remove local wallet", {exact:true}).click();
    await page.getByLabel("I have the recovery phrase").check();
    await page.getByRole("button", {name:"Remove from this browser",exact:true}).click();
    await status("Local wallet removed.").waitFor();
    await page.getByText("Restore an existing wallet", {exact:true}).click();
    await page.locator("#restore-words").fill(phrase);
    await page.locator("#birthday").fill("1");
    await page.getByRole("button", {name:"Restore wallet",exact:true}).click();
    await status("Wallet restored.").waitFor();
    assert.equal(await page.locator("#address").textContent(), address);
    assert.equal(await page.locator(".words").count(), 0, "restore invented a new phrase");

    // History navigation bypasses the link guard. Unacknowledged creation
    // must cancel without leaving a durable wallet behind.
    await page.getByText("Remove local wallet", {exact:true}).click();
    await page.getByLabel("I have the recovery phrase").check();
    await page.getByRole("button", {name:"Remove from this browser",exact:true}).click();
    await status("Local wallet removed.").waitFor();
    await page.getByRole("link", {name:"How it works",exact:true}).click();
    await page.getByRole("link", {name:"Back to wallet",exact:true}).click();
    await status("Ready to create or restore.").waitFor();
    await page.getByRole("button", {name:"Create wallet",exact:true}).click();
    await page.getByRole("heading", {name:"Save these 24 words."}).waitFor();
    await page.goBack();
    await page.getByRole("heading", {name:"Your browser is the wallet."}).waitFor();
    await page.getByRole("link", {name:"Back to wallet",exact:true}).click();
    await status("Ready to create or restore.").waitFor();
    assert.equal(await page.locator(".words").count(), 0, "cancelled creation retained its phrase");
    assert.equal(await page.locator("#address").count(), 0, "unconfirmed creation committed a wallet");
    // A full reload is also safe before acknowledgment.
    await page.getByRole("button", {name:"Create wallet",exact:true}).click();
    await page.getByRole("heading", {name:"Save these 24 words."}).waitFor();
    page.once("dialog", dialog => dialog.accept());
    await page.reload();
    await status("Ready to create or restore.").waitFor({timeout:90_000});
    assert.equal(await page.locator("#address").count(), 0);
    await page.getByRole("link", {name:"How it works",exact:true}).click();
    await page.getByRole("link", {name:"Back to wallet",exact:true}).click();
    await status("Ready to create or restore.").waitFor();

    // After acknowledgment, hold the actual IndexedDB completion callback
    // after physical commit, then tear down via browser history.
    await page.evaluate(() => {
      const original = IDBDatabase.prototype.transaction;
      window.__holdNextWalletSave = true;
      IDBDatabase.prototype.transaction = function (...args) {
        const tx = original.apply(this, args);
        if (args[1] !== "readwrite" || !window.__holdNextWalletSave) return tx;
        let walletSave = false, address;
        const objectStore = tx.objectStore.bind(tx);
        tx.objectStore = name => {
          const store = objectStore(name), put = store.put.bind(store);
          store.put = (value, key) => {
            if (key === "default") { walletSave = true; address = value.preview?.unifiedAddress; }
            return put(value, key);
          };
          return store;
        };
        Object.defineProperty(tx, "oncomplete", {
          configurable: true,
          set(callback) {
            tx.addEventListener("complete", event => {
              if (walletSave && window.__holdNextWalletSave) {
                window.__holdNextWalletSave = false;
                window.__heldWalletAddress = address;
                window.__releaseWalletCommit = () => {
                  IDBDatabase.prototype.transaction = original;
                  callback?.call(tx, event);
                };
              } else callback?.call(tx, event);
            }, {once:true});
          },
        });
        return tx;
      };
    });
    await page.getByRole("button", {name:"Create wallet",exact:true}).click();
    await page.getByRole("heading", {name:"Save these 24 words."}).waitFor();
    const confirmedPhrase = (await page.locator(".words li").allTextContents()).join(" ");
    assert.equal(confirmedPhrase.split(" ").length, 24);
    assert.equal(await page.evaluate(() => window.dispatchEvent(new Event("beforeunload", {cancelable:true}))), false);
    await page.getByLabel("I saved my recovery phrase").check();
    await page.getByRole("button", {name:"Done, hide phrase"}).click();
    await page.waitForFunction(() => !!window.__releaseWalletCommit);
    const committedAddress = await page.evaluate(() => window.__heldWalletAddress);
    assert.ok(committedAddress, "did not hold a durable wallet creation");
    await page.goBack();
    await page.getByRole("heading", {name:"Your browser is the wallet."}).waitFor();
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    await page.evaluate(() => window.__releaseWalletCommit());
    await page.getByRole("link", {name:"Back to wallet",exact:true}).click();
    await status("Wallet opened. Spending is locked.").waitFor({timeout:30_000});
    assert.equal(await page.locator("#address").textContent(), committedAddress);
    assert.equal(await page.locator(".words").count(), 0, "acknowledged phrase survived navigation");
    await page.getByRole("link", {name:"How it works",exact:true}).click();
    await page.getByRole("heading", {name:"Your browser is the wallet."}).waitFor();
    assert.equal(await page.evaluate(() => window.dispatchEvent(new Event("beforeunload", {cancelable:true}))), true,
      "acknowledgement left the unsaved-backup warning active");
    await page.getByRole("link", {name:"Back to wallet",exact:true}).click();
    await status("Wallet opened. Spending is locked.").waitFor();
    assert.ok(requests.every(request => !request.includes(confirmedPhrase)), "confirmed phrase left the browser");
    assert.ok(requests.every(request => !request.includes(phrase)), "phrase left the browser");
    assert.deepEqual(errors, [], "Next wallet had an unhandled runtime error");
    console.log(`Next.js ${name} ${isolated ? "MT" : "ST"}: create/backup/sync/error/reload/unlock/navigation/creation-teardown/diagnostics/forget/restore passed`);
  } catch (error) {
    try {
      console.error(`Next.js ${name} ${isolated ? "MT" : "ST"} status at failure:`,
        await page.locator(".wallet-main > .status").textContent({ timeout: 2000 }));
    } catch { /* The page may already be gone; never print phrase inputs. */ }
    throw error;
  } finally { await context.close(); await browser.close(); }
}
