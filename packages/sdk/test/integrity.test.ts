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
