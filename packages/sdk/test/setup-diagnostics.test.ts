import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { checkWalletSetup } from "../src/diagnostics.ts";

function browser(t: TestContext, overrides: Record<string, unknown> = {}) {
  for (const [name, value] of Object.entries({ window: {}, isSecureContext: true, indexedDB: {}, crossOriginIsolated: false, ...overrides })) {
    const old = Object.getOwnPropertyDescriptor(globalThis, name);
    Object.defineProperty(globalThis, name, { configurable: true, value });
    t.after(() => old ? Object.defineProperty(globalThis, name, old) : Reflect.deleteProperty(globalThis, name));
  }
}
test("server-safe import and default diagnostics never open storage, start workers or fetch", async t => {
  const fail = () => { throw new Error("unexpected side effect"); };
  browser(t, { fetch: fail, Worker: class { constructor() { fail(); } } });
  const report = await checkWalletSetup();
  assert.equal(report.ok, true);
  assert.equal(report.checks.find(check => check.code === "assets")?.status, "skipped");
  assert.equal(report.checks.find(check => check.code === "isolation")?.status, "warning");
});
test("asset checks detect wrong deployment bytes without disclosing URLs, hashes or errors", async t => {
  const wasm = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
  let mode = "valid";
  const hash = [...new Uint8Array(await crypto.subtle.digest("SHA-256", wasm))].map(byte => byte.toString(16).padStart(2, "0")).join("");
  browser(t, { fetch: async (url: string, options: RequestInit) => {
    assert.equal(options.credentials, "omit"); assert.equal(options.referrerPolicy, "no-referrer");
    assert.equal(options.redirect, "error"); assert.equal(options.cache, "no-store");
    if (mode === "private") throw new Error("private-sentinel");
    if (mode === "missing") return new Response("private-sentinel", { status: 404 });
    if (url.endsWith("manifest")) return Response.json({ sha256: mode === "mismatch" ? "0".repeat(64) : hash });
    return new Response(mode === "html" ? "private-sentinel" : wasm, { headers: { "content-type": mode === "html" ? "text/html" : "application/wasm" } });
  } });
  for (const [value, expected] of [["valid", "assets"], ["mismatch", "integrity_mismatch"], ["missing", "asset_missing"], ["html", "asset_format"], ["private", "asset_unavailable"]]) {
    mode = value;
    const report = await checkWalletSetup({ assets: { wasmUrl: "https://private-sentinel/wasm", integrityUrl: "https://private-sentinel/manifest" } });
    assert.ok(report.checks.some(check => check.code === expected && check.status === (value === "valid" ? "pass" : "fail")));
    assert.ok(!JSON.stringify(report).includes("private-sentinel"));
    assert.ok(!JSON.stringify(report).includes(hash));
  }
});
test("worker cancellation terminates a stalled probe and a stalled server has a bounded deadline", async t => {
  let terminated = 0;
  browser(t, { Worker: class { terminate() { terminated++; } } });
  const controller = new AbortController();
  const work = checkWalletSetup({ worker: true, signal: controller.signal });
  controller.abort();
  const report = await work;
  assert.equal(terminated, 1);
  assert.ok(report.checks.some(check => check.code === "cancelled"));
  const timed = await checkWalletSetup({ timeoutMs: 5, server: async () => new Promise(() => {}) });
  assert.equal(timed.ok, false);
  assert.ok(timed.checks.some(check => check.code === "server" && check.status === "fail"));
});
test("server probes expose only capability booleans and never raw provider diagnostics", async t => {
  browser(t);
  let inspected = false;
  const error = new Error();
  Object.defineProperty(error, "message", { get() { inspected = true; return "private-sentinel"; } });
  const report = await checkWalletSetup({ server: async () => { throw error; } });
  assert.equal(inspected, false); assert.ok(!JSON.stringify(report).includes("private-sentinel"));
});
