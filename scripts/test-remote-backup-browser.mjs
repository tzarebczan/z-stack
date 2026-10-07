import { launchBrowser } from "./browser-launch.mjs";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer, request as proxyRequest } from "node:http";
import { readFileSync, rmSync } from "node:fs";
import { resolve, sep } from "node:path";

/** Actual WebAuthn verifier + SQLite + browser crypto; no account-service fixtures. */
export async function verifyRemoteBackupBrowser(app, { chromium }) {
  let backendPort;
  const dist = resolve(app, "dist");
  const server = createServer((request, response) => {
    if (request.url.startsWith("/api/")) {
      const upstream = proxyRequest({ host: "127.0.0.1", port: backendPort, method: request.method, path: request.url,
        headers: request.headers }, received => { response.writeHead(received.statusCode, received.headers); received.pipe(response); });
      upstream.on("error", () => response.writeHead(502).end()); request.pipe(upstream); return;
    }
    const pathname = new URL(request.url, "http://localhost").pathname;
    const file = resolve(dist, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!file.startsWith(dist + sep)) { response.writeHead(403).end(); return; }
    try { response.setHeader("Content-Type", file.endsWith(".js") ? "application/javascript" : file.endsWith(".css") ? "text/css" : "text/html"); response.end(readFileSync(file)); }
    catch { response.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://localhost:${server.address().port}`;
  const backend = spawn(process.execPath, ["--import", "tsx", "server/main.ts"], { cwd: app, stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, NODE_OPTIONS: "", Z_STACK_BACKUP_EXAMPLE_ORIGIN: origin, Z_STACK_BACKUP_EXAMPLE_PORT: "0" } });
  let browser;
  const failures = [];
  const phrase = [...Array(23).fill("abandon"), "art"].join(" ");
  const ready = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("backup example startup timed out")), 15000);
    backend.stdout.on("data", chunk => { const match = /BACKUP_READY:(\d+)/.exec(String(chunk)); if (match) { backendPort = Number(match[1]); clearTimeout(timer); resolve(); } });
    backend.stderr.on("data", () => {}); // Never forward request/credential diagnostics.
    backend.once("exit", () => { clearTimeout(timer); reject(new Error("backup example exited before ready")); });
    backend.once("error", reject);
  });
  try {
    await ready; browser = await launchBrowser(chromium, { headless: true });
    const context = await browser.newContext(); const page = await context.newPage();
    page.on("pageerror", error => failures.push(error.message));
    await page.route("**/*", route => { assert.equal(new URL(route.request().url()).hostname, "localhost"); return route.continue(); });
    const cdp = await context.newCDPSession(page); await cdp.send("WebAuthn.enable");
    await cdp.send("WebAuthn.addVirtualAuthenticator", { options: { protocol: "ctap2", transport: "internal", hasResidentKey: true,
      hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true, hasPrf: true } });
    const wait = text => page.getByRole("status").filter({ hasText: text }).waitFor({ timeout: 30000 });
    await page.goto(origin); await page.locator("#phrase").fill(phrase);
    await page.locator("#prepare-register").click(); await wait("Ready.");
    await page.locator("#register").click(); await wait("Account verified.");
    // Commit the first upload but lose its acknowledgement. The UI must not
    // call this a backup until the user retries and exact readback confirms it.
    let lost = false;
    const dropReceipt = async route => {
      if (route.request().method() === "PUT" && !lost) {
        lost = true; await route.fetch(); await route.abort("failed");
      } else await route.continue();
    };
    await page.route("**/api/backups/example-wallet", dropReceipt);
    await page.locator("#upload").click(); await wait("Action incomplete.");
    await page.unroute("**/api/backups/example-wallet", dropReceipt);
    await page.locator("#upload").click(); await wait("Backup confirmed:");
    // Readback is encrypted; ownership ignores any caller-supplied account identity.
    const exported = await page.evaluate(async () => {
      const result = await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } }); return result.json();
    });
    assert.ok(exported.data.ct); assert.ok(!JSON.stringify(exported).includes(phrase));
    const stale = await page.evaluate(async record => {
      const result = await fetch('/api/backups/example-wallet', { method: 'PUT', headers: { 'X-Example-Request': '1', 'Content-Type': 'application/json' }, body: JSON.stringify({ record }) });
      return result.status;
    }, exported);
    assert.equal(stale, 409);
    // A second real verified account gets a distinct owner even though the UI
    // uses the same backup id. Caller-provided ownership cannot cross accounts.
    const otherContext = await browser.newContext();
    try {
      const other = await otherContext.newPage();
      other.on("pageerror", error => failures.push(error.message));
      const otherCdp = await otherContext.newCDPSession(other); await otherCdp.send("WebAuthn.enable");
      await otherCdp.send("WebAuthn.addVirtualAuthenticator", { options: { protocol: "ctap2", transport: "internal", hasResidentKey: true,
        hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true, hasPrf: true } });
      await other.goto(origin); await other.locator("#phrase").fill(phrase);
      await other.locator("#prepare-register").click(); await other.getByRole("status").filter({ hasText: "Ready." }).waitFor();
      await other.locator("#register").click(); await other.getByRole("status").filter({ hasText: "Account verified." }).waitFor();
      assert.equal(await other.evaluate(async () => (await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } })).status), 404);
      assert.equal(await other.evaluate(async record => (await fetch('/api/backups/example-wallet', { method: 'PUT', headers: { 'X-Example-Request': '1', 'Content-Type': 'application/json' },
        body: JSON.stringify({ record }) })).status, exported), 400);
      await other.locator("#delete-account").click();
      await other.getByRole("status").filter({ hasText: "Account, credentials, sessions and remote backup deleted." }).waitFor();
      assert.equal(await page.evaluate(async () => (await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } })).status), 200);
    } finally { await otherContext.close(); }

    await page.locator("#forget-local").click(); await wait("Local vault forgotten.");
    await page.locator("#logout").click(); await wait("Signed out.");
    // Drop all browser storage/cookies but retain the authenticator, as with a
    // synced credential. No encrypted local copy can satisfy this recovery.
    await cdp.send("Storage.clearDataForOrigin", { origin, storageTypes: "all" });
    await page.reload();
    const unauthenticated = await page.evaluate(async () => (await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } })).status);
    assert.equal(unauthenticated, 401);
    await page.locator("#prepare-sign-in").click(); await wait("Ready.");
    await page.locator("#sign-in").click(); await wait("Signed in.");
    await page.locator("#retrieve").click(); await wait("Encrypted backup restored.");
    await page.locator("#unlock").click(); await wait("Recovered locally.");
    // Commit the tombstone but drop its response. A retry must confirm the
    // existing deletion rather than report a revision conflict indefinitely.
    lost = false;
    await page.route("**/api/backups/example-wallet", dropReceipt);
    await page.locator("#forget-remote").click(); await wait("Action incomplete.");
    await page.unroute("**/api/backups/example-wallet", dropReceipt);
    await page.locator("#forget-remote").click(); await wait("Remote backup forgotten.");
    const tombstone = await page.evaluate(async () => (await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } })).json());
    assert.equal(tombstone.forgotten, true); assert.equal(tombstone.data.ct, "");
    await page.locator("#retrieve").click(); await wait("Remote backup forgotten. Local copies remain.");
    await page.locator("#delete-account").click(); await wait("Account, credentials, sessions and remote backup deleted.");
    assert.equal(await page.evaluate(async () => (await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } })).status), 401);
    const noPrfContext = await browser.newContext();
    try {
      const noPrf = await noPrfContext.newPage();
      noPrf.on("pageerror", error => failures.push(error.message));
      let verifications = 0;
      noPrf.on("request", request => { if (request.url().endsWith("/api/register/verify")) verifications++; });
      const noPrfCdp = await noPrfContext.newCDPSession(noPrf); await noPrfCdp.send("WebAuthn.enable");
      await noPrfCdp.send("WebAuthn.addVirtualAuthenticator", { options: { protocol: "ctap2", transport: "internal", hasResidentKey: true,
        hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true, hasPrf: false } });
      await noPrf.goto(origin); await noPrf.locator("#phrase").fill(phrase);
      await noPrf.locator("#prepare-register").click(); await noPrf.getByRole("status").filter({ hasText: "Ready." }).waitFor();
      await noPrf.locator("#register").click();
      await noPrf.getByRole("status").filter({ hasText: "cannot encrypt a vault" }).waitFor();
      assert.equal(verifications, 0, "non-PRF local protection must not verify an account");
      assert.equal(await noPrf.locator("#phrase").inputValue(), phrase, "failed protection discarded the recovery phrase");
      assert.equal(await noPrf.evaluate(async () => (await fetch('/api/backups/example-wallet', { headers: { 'X-Example-Request': '1' } })).status), 401);
    } finally { await noPrfContext.close(); }
    assert.deepEqual(failures, []);
    console.log("Remote backup: real WebAuthn verification, encrypted upload/readback, lost-acknowledgement reconciliation, account isolation, stale-write rejection, storage-loss recovery, tombstone and account deletion passed");
  } finally {
    await browser?.close(); backend.kill("SIGTERM");
    if (backend.exitCode === null) await new Promise(resolve => { backend.once("exit", resolve); setTimeout(() => { backend.kill("SIGKILL"); resolve(); }, 3000).unref(); });
    await new Promise(resolve => server.close(resolve)); rmSync(resolve(app, ".data"), { recursive: true, force: true });
  }
}
