import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve, sep } from "node:path";
import { assertLocalRegtestChain } from "./sdk-harness.mjs";
import { generate, waitForZebra, zebraRpc, ZEBRA_RPC } from "./regtest-rpc.mjs";
import { createGrpcWebProxy } from "./grpc-web-proxy.mjs";

/** Installed archives, real WASM and a loopback chain; no application .env or private seed. */
export async function verifyFundedBrowser(app, browsers) {
  const info = await waitForZebra(20_000);
  assertLocalRegtestChain(info, ZEBRA_RPC);
  const upstream = process.env.Z_STACK_REGTEST_LWD ?? "http://127.0.0.1:28137";
  assert.ok(["127.0.0.1", "localhost", "[::1]"].includes(new URL(upstream).hostname), "funded fixture must use loopback");
  const original = readFileSync(join(app, "index.html"));
  let browser, proxy;
  const dist = resolve(app, "dist");
  let pageOrigin;
  const server = createServer(async (req, res) => {
    // Zaino 0.10's regtest submission is broken; the native harness uses the
    // same explicit loopback validator fallback. This is fixture-only code.
    if (req.url === "/fixture-submit" && req.method === "POST") {
      if (req.headers.origin !== pageOrigin || req.headers["x-fixture"] !== "1") { res.writeHead(403).end(); return; }
      try {
        let body = "";
        for await (const chunk of req) { body += chunk.toString(); if (body.length > 4_000_000) throw new Error("fixture request too large"); }
        const { rawHex } = JSON.parse(body);
        assert.ok(typeof rawHex === "string" && /^(?:[a-f0-9]{2})+$/i.test(rawHex));
        assertLocalRegtestChain(await zebraRpc("getblockchaininfo"), ZEBRA_RPC);
        const txid = await zebraRpc("sendrawtransaction", [rawHex]);
        res.setHeader("Content-Type", "application/json"); res.setHeader("Cache-Control", "no-store");
        res.end(JSON.stringify({ txid }));
      } catch { res.writeHead(502).end(); }
      return;
    }
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    const pathname = new URL(req.url, "http://localhost").pathname;
    const path = resolve(dist, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!path.startsWith(dist + sep)) { res.writeHead(403).end(); return; }
    try {
      res.setHeader("Content-Type", path.endsWith(".wasm") ? "application/wasm" : path.endsWith(".js") ? "application/javascript" : "text/html");
      res.end(readFileSync(path));
    } catch { res.writeHead(404).end(); }
  });
  try {
    await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
    const origin = `http://127.0.0.1:${server.address().port}`; pageOrigin = origin;
    proxy = createGrpcWebProxy({ upstream, origins: [origin], allowTransparent: true, timeoutMs: 180_000 });
    await new Promise(resolve => proxy.listen(0, "127.0.0.1", resolve));
    const chainUrl = `http://127.0.0.1:${proxy.address().port}`;
    const height = Number(process.env.Z_STACK_REGTEST_NU6_3 || 1_000_000);
    writeFileSync(join(app, "src", "funded-check.ts"), `
      import { createWallet, grpcWebTransport } from '@z-stack/sdk';
      const REGTEST_FAUCET_MNEMONIC = 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about';
      const transport = grpcWebTransport(${JSON.stringify(chainUrl)}, { transparent: true });
      transport.submit = async rawHex => {
        const response = await fetch('/fixture-submit', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Fixture': '1' },
          body: JSON.stringify({ rawHex }) });
        if (!response.ok) throw new Error('local regtest submission failed');
        return (await response.json()).txid;
      };
      const wallet = await createWallet({ network: 'regtest', server: transport,
        regtestNu63Height: ${height}, ${process.env.Z_STACK_REGTEST_NU7 === undefined ? "" : `regtestNu7Height:${Number(process.env.Z_STACK_REGTEST_NU7)},`} threads: 2, transparent: true, transparentScan: 'compact', autoShield: false,
        memoFetch: 'on-demand', prewarmProvingKey: false });
      const saved = await wallet.load();
      if (!saved) await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1 });
      Object.assign(window, { fundedWallet: wallet, fundedSeed: REGTEST_FAUCET_MNEMONIC, fundedReady: true });
    `);
    writeFileSync(join(app, "index.html"), '<!doctype html><html><head><link rel="icon" href="data:,"></head><body><script type="module" src="/src/funded-check.ts"></script></body></html>');
    const built = spawnSync("npm", ["run", "build"], { cwd: app, stdio: "inherit" });
    assert.equal(built.status, 0);
    browser = await browsers.chromium.launch({ headless: true });
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.route("**/*", route => {
      assert.equal(new URL(route.request().url()).hostname, "127.0.0.1", "funded fixture contacted a remote host");
      return route.continue();
    });
    await page.goto(origin);
    await page.waitForFunction(() => window.fundedReady, null, { timeout: 120_000 });
    async function syncToTip() {
      const target = Number((await zebraRpc("getblockchaininfo")).blocks);
      const deadline = Date.now() + 180_000;
      for (;;) {
        const scanned = await page.evaluate(async () => (await window.fundedWallet.sync()).scannedHeight);
        if (scanned >= target) return;
        assert.ok(Date.now() < deadline, "Zaino/browser failed to catch up");
        await new Promise(resolve => setTimeout(resolve, 500));
      }
    }
    await syncToTip();
    const available = await page.evaluate(async () => (await window.fundedWallet.getWallet()).balance.transparentAvailable);
    assert.ok(available >= 100_000, "fixture needs matured transparent coinbase funds");
    const shield = await page.evaluate(async () => {
      await window.fundedWallet.unlock(window.fundedSeed);
      return window.fundedWallet.shield();
    });
    assert.ok(shield.txid, "shield did not broadcast");
    await generate(20);
    await syncToTip();
    const sent = await page.evaluate(async () => {
      const wallet = window.fundedWallet;
      const { unifiedAddress } = await wallet.nextAddress();
      await wallet.unlock(window.fundedSeed);
      const balance = (await wallet.getWallet()).balance;
      const shieldedBefore = balance.saplingAvailable + balance.orchardAvailable + balance.ironwoodAvailable;
      const fee = await wallet.estimateFee(unifiedAddress, '0.0005', 'packaged-sdk-regtest');
      const receipt = await wallet.send(unifiedAddress, '0.0005', 'packaged-sdk-regtest');
      return { ...receipt, shieldedBefore, quotedFeeZat: fee.feeZat };
    });
    assert.ok(sent.txid, "send did not return a receipt");
    assert.ok(await page.evaluate(async txid => (await window.fundedWallet.pending()).some(tx => tx.txid === txid), sent.txid));
    await generate(15);
    await syncToTip();
    await page.evaluate(() => window.fundedWallet.fetchMemos());
    const received = await page.evaluate(txid => window.fundedWallet.transaction(txid), sent.txid);
    assert.equal(received?.status, "mined");
    const shieldedAfter = await page.evaluate(async () => {
      const b = (await window.fundedWallet.getWallet()).balance;
      return b.saplingAvailable + b.orchardAvailable + b.ironwoodAvailable;
    });
    assert.equal(sent.shieldedBefore - shieldedAfter, sent.quotedFeeZat,
      "A mined self-payment must debit only the quoted fee, including after NU7/NSM");
    assert.ok(received.memos?.includes("packaged-sdk-regtest"), "memo was not recovered");
    assert.equal(await page.evaluate(async txid => (await window.fundedWallet.pending()).some(tx => tx.txid === txid), sent.txid), false);
    assert.equal(await page.evaluate(() => window.fundedWallet.runtime.mode), "multi-thread");
    await page.evaluate(() => window.fundedWallet.close());
    await page.reload();
    await page.waitForFunction(() => window.fundedReady, null, { timeout: 120_000 });
    assert.equal((await page.evaluate(txid => window.fundedWallet.transaction(txid), sent.txid))?.status, "mined");
    assert.equal(await page.evaluate(() => window.fundedWallet.hasSpendingSeed()), false, "reload retained spending seed");
    await page.evaluate(() => window.fundedWallet.close());
    assert.deepEqual(errors, []);
    console.log("Funded installed SDK: restore, transparent shield, local proof, send, pending/mined, memo, close/reload and locked seed passed");
  } finally {
    if (browser) await browser.close();
    if (proxy) { proxy.closeAllConnections(); await new Promise(resolve => proxy.close(resolve)); }
    server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
    writeFileSync(join(app, "index.html"), original);
  }
}
