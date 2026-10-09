import assert from "node:assert/strict";
import { test } from "node:test";
import { allowMissingBuiltWasm, assertSha256, sha256Hex, verifyWasmAt } from "../src/integrity.ts";

test("sha256 of empty input", async () => {
  assert.equal(
    await sha256Hex(new Uint8Array()),
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  );
});

test("assertSha256 rejects a different blob", async () => {
  const hex = await sha256Hex(new Uint8Array([1]));
  await assert.rejects(() => assertSha256(new Uint8Array([2]), hex), /mismatch/);
});

test("verified loader rejects a mismatched artifact before instantiation", async (t) => {
  t.mock.method(globalThis, "fetch", async (url: string) => url.endsWith(".json")
    ? Response.json({ sha256: await sha256Hex(new Uint8Array([1])) })
    : new Response(new Uint8Array([2])));
  await assert.rejects(verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json"), /mismatch/);
});

test("a failed integrity manifest does not skip the check", async (t) => {
  t.mock.method(globalThis, "fetch", async () => {
    throw new TypeError("blocked");
  });
  await assert.rejects(
    verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json"),
    /blocked/,
  );
});

test("a dev opt-out never applies to an explicit wasm base", () => {
  assert.equal(allowMissingBuiltWasm(true), false);
  assert.equal(allowMissingBuiltWasm(false), false);
});

test("missing integrity manifest leaves the single WASM fetch to the loader", async (t) => {
  let calls = 0;
  t.mock.method(globalThis, "fetch", async () => { calls++; return new Response(null, { status: 404 }); });
  await assert.rejects(
    verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json"),
    /integrity manifest missing/,
  );
  assert.equal(
    await verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json", { allowMissing: true }),
    undefined,
  );
  assert.equal(calls, 2);
});

test("an inlined data: manifest is decoded without fetch (strict CSP connect-src)", async (t) => {
  const { verifyWasmAt, sha256Hex } = await import("../src/integrity.ts");
  const wasm = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
  const manifest = { sha256: await sha256Hex(wasm) };
  const dataUrl = `data:application/json;base64,${Buffer.from(JSON.stringify(manifest)).toString("base64")}`;
  const urls: string[] = [];
  t.mock.method(globalThis, "fetch", async (url: string) => {
    urls.push(String(url));
    if (String(url).startsWith("data:")) throw new TypeError("blocked by CSP connect-src");
    return new Response(wasm);
  });
  const bytes = await verifyWasmAt("https://app.example/z_wasm_bg.wasm", dataUrl);
  assert.ok(bytes, "verification ran");
  assert.deepEqual(urls, ["https://app.example/z_wasm_bg.wasm"]);
});

test("streamed startup reports decoded bytes and still verifies the exact artifact", async t => {
  const chunks = [new Uint8Array([0, 97]), new Uint8Array([115, 109, 1])];
  const expected = new Uint8Array([0, 97, 115, 109, 1]);
  const digest = await sha256Hex(expected);
  const progress: { phase: string; loadedBytes: number; totalBytes?: number }[] = [];
  t.mock.method(globalThis, "fetch", async (url: string) => url.endsWith(".json") ? Response.json({ sha256: digest }) :
    new Response(new ReadableStream({ start(controller) { chunks.forEach(chunk => controller.enqueue(chunk)); controller.close(); } }),
      { headers: { "Content-Encoding": "gzip", "Content-Length": "3" } }));
  const bytes = await verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json", { onProgress: value => progress.push(value) });
  assert.deepEqual(new Uint8Array(bytes!), expected);
  assert.equal(progress.at(-1)?.phase, "verify");
  assert.equal(progress.at(-1)?.loadedBytes, 5);
  assert.ok(progress.every(value => value.totalBytes === undefined), "compressed lengths must not become decoded-byte totals");
});

test("cross-origin progress does not trust a length when compression headers may be hidden", async t => {
  const expected = new Uint8Array([0, 97, 115, 109, 1]);
  const digest = await sha256Hex(expected);
  const progress: { loadedBytes: number; totalBytes?: number }[] = [];
  t.mock.method(globalThis, "fetch", async (url: string) => {
    if (url.endsWith(".json")) return Response.json({ sha256: digest });
    const response = new Response(expected, { headers: { "Content-Length": "3" } });
    Object.defineProperty(response, "type", { value: "cors" });
    return response;
  });
  await verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json", { onProgress: value => progress.push(value) });
  assert.equal(progress.at(-1)?.loadedBytes, 5);
  assert.ok(progress.every(value => value.totalBytes === undefined));
});

test("progress handler failures cannot bypass integrity or discard verified bytes", async t => {
  t.mock.method(console, "warn", () => {});
  let expected = await sha256Hex(new Uint8Array([1]));
  t.mock.method(globalThis, "fetch", async (url: string) => url.endsWith(".json") ? Response.json({ sha256: expected }) :
    new Response(new Uint8Array([2]), { headers: { "Content-Length": "1" } }));
  const options = { onProgress() { throw new Error("SYNTHETIC_PRIVATE_PAYLOAD"); } };
  await assert.rejects(verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json", options), /mismatch/);
  expected = await sha256Hex(new Uint8Array([2]));
  assert.deepEqual(new Uint8Array((await verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json", options))!), new Uint8Array([2]));
});

test("an interrupted download never returns partial bytes or reports verification", async t => {
  const progress: string[] = [];
  const digest = await sha256Hex(new Uint8Array([0, 97, 115, 109]));
  let pulls = 0;
  const response = new Response(new ReadableStream({ pull(controller) {
    if (++pulls === 1) controller.enqueue(new Uint8Array([0, 97]));
    else controller.error(new Error("Interrupted fixture download"));
  } }));
  t.mock.method(globalThis, "fetch", async (url: string) => url.endsWith(".json") ? Response.json({ sha256: digest }) : response);
  await assert.rejects(verifyWasmAt("https://example.invalid/app.wasm", "https://example.invalid/integrity.json", {
    onProgress: value => progress.push(value.phase),
  }), /Interrupted fixture download/);
  assert.ok(!progress.includes("verify"));
  assert.equal(response.body?.locked, false);
});
