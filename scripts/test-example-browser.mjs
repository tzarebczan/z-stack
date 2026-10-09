import { launchBrowser } from "./browser-launch.mjs";
import { assertSavedHide, verifyBackForward } from "./browser-hide-lifecycle.mjs";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { readFileSync, writeFileSync, unlinkSync } from "node:fs";
import { join, resolve, sep } from "node:path";

/** Run the actual production example, without replacing the SDK or its engine. */
export async function verifyExampleBrowser(app, browsers, example, browserName = "chromium") {
  const chromium = browsers[browserName];
  const dist = resolve(app, "dist");
  const server = createServer((req, res) => {
    const pathname = new URL(req.url, "http://localhost").pathname;
    const file = resolve(dist, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!file.startsWith(dist + sep)) { res.writeHead(403).end(); return; }
    try {
      res.setHeader("Content-Type", file.endsWith(".wasm") ? "application/wasm" :
        file.endsWith(".js") ? "application/javascript" : file.endsWith(".css") ? "text/css" : "text/html");
      res.end(readFileSync(file));
    } catch { res.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const browser = await launchBrowser(chromium, { headless: true });
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    const remoteRequests = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.route("**/*", route => {
      if (new URL(route.request().url()).hostname !== "127.0.0.1") {
        remoteRequests.push(route.request().url());
        return route.abort();
      }
      return route.continue();
    });
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.getByRole("status").filter({ hasText: "Create a testnet wallet." }).waitFor({ timeout: 90_000 });
    if (example === "react-wallet") {
      for (let remount = 0; remount < 3; remount++) {
        await page.getByRole("button", { name: "Close wallet screen" }).click();
        await page.getByRole("button", { name: "Open wallet screen" }).click();
        await page.getByRole("status").filter({ hasText: "Create a testnet wallet." }).waitFor({ timeout: 90_000 });
      }
    } else {
      assert.equal(await page.getByRole("button", { name: "Protect with passkey" }).isDisabled(), true);
      assert.equal(await page.getByRole("button", { name: "Unlock with passkey" }).isDisabled(), true);
    }
    assert.deepEqual(remoteRequests, [], "opening a local wallet example must not contact a remote service");
    assert.deepEqual(errors, [], "example reported an uncaught runtime error");
    await context.close();
    await verifyRecovery(app, browser, server.address().port, example);
    console.log(`Production ${example}: real engine startup${example === "react-wallet" ? ", StrictMode and remounts" : ""}, no remote service requests`);
  } finally {
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
}


/** Change only the app-owned chain connection in the disposable installed consumer. */
async function verifyRecovery(app, browser, port, example) {
  const path = join(app, "src", example === "react-wallet" ? "WalletPanel.tsx" : "main.ts");
  const source = readFileSync(path, "utf8");
  const fixture = join(app, "src", "wallet-fixture.ts");
  writeFileSync(path, source.replace('from "@z-stack/sdk";', 'from "./wallet-fixture";'));
  writeFileSync(fixture, `
    import { createWallet as realCreateWallet } from '@z-stack/sdk';
    export * from '@z-stack/sdk';
    export function createWallet(options: Parameters<typeof realCreateWallet>[0]) {
      return realCreateWallet({...options, network: 'regtest', prewarmProvingKey: false,
        server: {kind:'example-recovery-fixture', label:'Offline compact-block fixture',
          tip: async () => 1, blocks: async () => {throw new Error('Unexpected sync');}}});
    }
  `);
  let context;
  try {
    const build = spawnSync("npm", ["run", "build"], {cwd:app, stdio:"inherit", shell:process.platform === "win32"});
    assert.equal(build.status, 0, "recovery consumer failed to build");
    context = await browser.newContext();
    const page = await context.newPage(), requests = [], errors = [], diagnostics = [];
    page.setDefaultTimeout(30_000);
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => diagnostics.push(message.text()));
    page.on("request", request => requests.push(request.url() + (request.postData() ?? "")));
    const origin = `http://127.0.0.1:${port}`;
    await page.route("**/*", route => {
      assert.equal(new URL(route.request().url()).origin, origin, "recovery contacted a remote service");
      return route.continue();
    });
    const ready = () => page.getByRole("status").filter({hasText:"Create a testnet wallet."}).waitFor({timeout:90_000});
    const create = () => page.getByRole("button", {name:"Create wallet",exact:true}).click();
    const phrase = page.locator("#phrase");
    await page.goto(origin); await ready();
    if (example === "react-wallet") {
      await page.evaluate(() => {
        window.dispatchEvent(new PageTransitionEvent("pagehide", { persisted: true }));
        window.dispatchEvent(new PageTransitionEvent("pageshow", { persisted: true }));
      });
      await ready();
      assert.equal((await page.locator("#address").textContent()).trim(), "");
    }
    if (example === "react-wallet") {
      await create(); await phrase.waitFor({state:"visible"});
      await page.getByRole("button", {name:"Close wallet screen"}).click();
      await page.getByRole("button", {name:"Open wallet screen"}).click(); await ready();
      assert.equal((await page.locator("#address").textContent()).trim(), "", "unconfirmed unmount saved a wallet");
    }
    await create(); await phrase.waitFor({state:"visible"});
    assert.equal((await phrase.textContent()).trim().split(/\s+/).length, 24);
    if (example === "local-passkey") assert.equal(await page.getByRole("button", {name:"Protect with passkey"}).isDisabled(), true);
    page.once("dialog", dialog => dialog.accept());
    await page.reload(); await ready();
    assert.equal((await page.locator("#address").textContent()).trim(), "", "unconfirmed reload saved a wallet");
    await create(); await phrase.waitFor({state:"visible"});
    const recovery = await phrase.textContent();
    assert.equal(recovery.trim().split(/\s+/).length, 24);
    await page.getByRole("button", {name:"I saved these words — finish",exact:true}).click();
    await page.getByRole("status").filter({hasText:"Wallet created."}).waitFor();
    const address = await page.locator("#address").textContent(); assert.ok(address);
    if (example === "local-passkey") {
      assert.equal(await page.getByRole("button", {name:"Protect with passkey"}).isEnabled(), true);
      await page.getByRole("button", {name:"Hide recovery phrase",exact:true}).click();
    }
    await page.reload();
    await page.getByRole("status").filter({hasText:"Wallet opened."}).waitFor({timeout:90_000});
    assert.equal(await page.locator("#address").textContent(), address);
    for (let reload = 0; reload < 3; reload++) {
      await page.reload();
      await page.getByRole("status").filter({hasText:"Wallet opened."}).waitFor({timeout:90_000});
      assert.equal(await page.locator("#address").textContent(), address);
    }
    await assertSavedHide(page, address);
    if (example === "react-wallet") {
      await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent("pageshow", { persisted: true })));
      await page.getByRole("status").filter({hasText:"Wallet opened."}).waitFor();
      await page.getByRole("button", {name:"Lock",exact:true}).click();
      await page.getByRole("status").filter({hasText:"Spending locked."}).waitFor();
      await verifyBackForward(page, () => page.getByRole("status").filter({hasText:"Wallet opened."}).waitFor({timeout:90_000}), address);
    } else {
      await page.reload();
      await page.getByRole("status").filter({hasText:"Wallet opened."}).waitFor({timeout:90_000});
    }
    assert.ok(!diagnostics.some(value => value.includes("snapshot save on hide failed")), "hide must leave the scan worker available for its snapshot flush");
    assert.ok(!(await page.locator("body").textContent()).includes(recovery));
    assert.ok(requests.every(request => !request.includes(recovery)), "recovery phrase left the browser");
    assert.deepEqual(errors, []);
    console.log(`${example}: real creation confirms recovery before commit; unconfirmed teardown cancels; confirmed reload stays locked`);
  } finally {
    await context?.close(); writeFileSync(path, source); unlinkSync(fixture);
  }
}
