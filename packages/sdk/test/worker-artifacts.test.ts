import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { setImmediate as turn } from "node:timers/promises";
import { test } from "node:test";
import { sha256Hex } from "../src/integrity.ts";

test("custom relative artifacts propagate to UI and both workers with integrity and ST selection", async (t) => {
  const artifact = new URL("../src/generated/z_wasm_bg.wasm", import.meta.url);
  if (!existsSync(artifact)) { t.skip("run pnpm build:wasm first"); return; }
  const bytes = readFileSync(artifact);
  const sha256 = await sha256Hex(bytes);
  const base = "https://fixture.invalid/app/custom";
  const urls = [`${base}/integrity.json`, `${base}/z_wasm_bg.wasm`];
  const requests: string[] = [];
  let badHash = false;
  let badMtHash = false;
  t.mock.method(globalThis, "fetch", async (input: string | URL) => {
    const url = String(input);
    requests.push(url);
    if (url.endsWith("integrity.json")) {
      return Response.json({ sha256: badHash || (badMtHash && url.includes("generated-mt")) ? "0".repeat(64) : sha256 });
    }
    return new Response(bytes);
  });
  const globals = ["window", "document", "Worker", "self", "crossOriginIsolated"];
  const originals = globals.map((key) => Object.getOwnPropertyDescriptor(globalThis, key));
  const set = (key: string, value: unknown) => Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  t.after(() => globals.forEach((key, i) => {
    if (originals[i]) Object.defineProperty(globalThis, key, originals[i]!); else Reflect.deleteProperty(globalThis, key);
  }));
  const workers: FixtureWorker[] = [];
  class FixtureWorker extends EventTarget {
    messages: Record<string, unknown>[] = [];
    constructor(readonly url: URL) { super(); workers.push(this); }
    postMessage(msg: Record<string, unknown>) {
      this.messages.push(msg);
      queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: {
        id: msg.id, mode: "single-thread", threads: 1, ready: false,
      } })));
    }
    terminate() {}
  }
  set("window", new EventTarget());
  set("document", { baseURI: "https://fixture.invalid/app/index.html" });
  set("Worker", FixtureWorker);
  set("crossOriginIsolated", true);
  const sdk = await import("../src/lab.ts");
  await sdk.initialize({ wasmBasePath: "./custom/", preferMulticore: true, prewarmProvingKey: false, regtestNu63Height:150, regtestNu7Height:250 });
  await sdk.initialize({ regtestNu7Height:250 });
  await assert.rejects(sdk.initialize({ regtestNu7Height:251 }), /different regtestNu7Height/);
  await turn();
  assert.deepEqual(requests.splice(0), urls, "UI resolves the custom location against the document");
  assert.equal(sdk.wasmRuntime()?.mode, "single-thread");
  assert.equal(workers.length, 2);
  const scan = workers.find((worker) => worker.url.href.includes("scan.worker"))!;
  const prove = workers.find((worker) => worker.url.href.includes("prove.worker"))!;
  assert.equal(scan.messages[0].wasmBasePath, base);
  assert.equal(scan.messages[0].regtestNu7Height, 250);
  assert.equal(prove.messages[0].regtestNu7Height, 250);
  assert.equal(scan.messages[0].preferMulticore, false, "custom ST artifact never guesses an MT companion");
  assert.equal(prove.messages[0].wasmBasePath, base);
  const { restartScanWorker } = await import("../src/scan-host.ts");
  await restartScanWorker();
  assert.equal(workers.at(-1)!.messages[0].regtestNu7Height,250,"scan restarts retain NU7 scheduling");
  assert.equal(workers.at(-1)!.messages[0].wasmBasePath, base, "scan restarts retain the same artifact source");

  // Exercise the actual worker entry points, not just the host's message shape.
  // Node shares the generated JS module instance; browser isolation/MT execution
  // is separately covered by the real-browser regtest harness.
  function workerScope() {
    let onReply: ((value: Record<string, unknown>) => void) | undefined;
    const scope = Object.assign(new EventTarget(), { onmessage: undefined as undefined | ((event: MessageEvent) => void),
      postMessage: (value: Record<string, unknown>) => onReply?.(value) });
    set("self", scope);
    return {
      request(msg: Record<string, unknown>): Promise<Record<string, unknown>> {
        return new Promise((accept, reject) => {
          const timer = setTimeout(() => reject(new Error("worker fixture did not reply")), 5000);
          onReply = (value) => { clearTimeout(timer); accept(value); };
          scope.onmessage!(new MessageEvent("message", { data: msg }));
        });
      },
    };
  }
  const scanScope = workerScope();
  await import("../src/scan.worker.ts");
  badHash = true;
  assert.match(String((await scanScope.request({ ...scan.messages[0], id: 1 })).error), /sha256 mismatch/);
  assert.deepEqual(requests.splice(0), urls, "scan worker verifies the custom bytes before use");
  badHash = false;
  assert.equal((await scanScope.request({ ...scan.messages[0], id: 2, preferMulticore: true })).mode, "single-thread");
  assert.deepEqual(requests.splice(0), urls, "even a direct MT preference keeps custom artifacts on the ST ABI");
  badMtHash = true;
  t.mock.method(console, "warn", () => {});
  const bundled = await scanScope.request({ id: 3, op: "init", preferMulticore: true, threads: 2 });
  assert.equal(bundled.mode, "single-thread", "failed bundled MT integrity falls back to verified bundled ST");
  assert.deepEqual(requests.splice(0).map((url) => url.split("/src/")[1]), [
    "generated-mt/integrity.json", "generated-mt/z_wasm_bg.wasm", "generated/integrity.json", "generated/z_wasm_bg.wasm",
  ]);

  const proveScope = workerScope();
  await import("../src/prove.worker.ts");
  badHash = true;
  assert.match(String((await proveScope.request({ ...prove.messages[0], id: 4 })).error), /sha256 mismatch/);
  assert.deepEqual(requests.splice(0), urls, "prove worker verifies the same custom artifact");
  badHash = false;
  assert.equal((await proveScope.request({ ...prove.messages[0], id: 5 })).ready, false);
  assert.deepEqual(requests.splice(0), urls);
  assert.equal((await proveScope.request({ id: 6, kind: "ready", wasmBasePath: base })).ready, false);
  assert.match(String((await proveScope.request({ id: 8, kind: "ready", wasmBasePath: base, regtestNu7Height: 251 })).error), /another regtestNu7Height/);
  assert.deepEqual(requests.splice(0), [], "prove worker reuses its verified initialization");
  assert.match(String((await proveScope.request({ id: 7, kind: "init", wasmBasePath: `${base}-other` })).error), /another wasmBasePath/);
  assert.deepEqual(requests, [], "an initialized proving worker cannot silently switch artifacts");
});
