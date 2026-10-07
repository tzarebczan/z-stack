import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { setImmediate as turn } from "node:timers/promises";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { attachWasmBindings, createWasmClient, forgetWasmWallet } from "../src/wasm-client.ts";
import { attachScanWorker, scanWorkerReady, scanWorkerBusy, workerScanSession, recoverScanWorker } from "../src/scan-host.ts";
import { advanceWalletGeneration, readSavedSnapshot, saveWalletSnapshot } from "../src/snapshot-storage.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";
import type { LwdTransport } from "../src/lwd.ts";

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(r => { resolve = r; });
  return { promise, resolve };
};
const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const decode = (bytes: Uint8Array | ArrayBuffer) => JSON.parse(new TextDecoder().decode(bytes));
const state = (name: string) => ({ unifiedAddress: name, network: "regtest", birthday: 1, scannedHeight: 10, pending: false });
type State = ReturnType<typeof state>;
const preview = (s: State) => ({ ...s, birthdayHeight: s.birthday,
  balance: { totalAvailable: s.pending ? 0 : 10, orchardAvailable: s.pending ? 0 : 10, transparentAvailable: 0 } });

function fixture(t: TestContext) {
  const db = memoryIndexedDb();
  const restore: Array<() => void> = [];
  const install = (name: string, value: unknown) => {
    const old = Object.getOwnPropertyDescriptor(globalThis, name);
    Object.defineProperty(globalThis, name, { value, configurable: true });
    restore.push(() => old ? Object.defineProperty(globalThis, name, old) : Reflect.deleteProperty(globalThis, name));
  };
  install("indexedDB", db.indexedDB);
  install("IDBKeyRange", db.IDBKeyRange);
  const make = (initial: State) => {
    const s = { ...initial };
    let freed = false;
    return {
      free() { assert.equal(freed, false); freed = true; },
      toSnapshot: () => encode(s), snapshotJson: () => JSON.stringify(preview(s)),
      scannedHeight: () => s.scannedHeight, history: () => "[]", nextUnifiedAddress() {},
      resetScan() { s.scannedHeight = 0; s.pending = false; },
    };
  };
  attachWasmBindings({ generateMnemonic: () => "fixture", WasmWallet: {
    create: (_net: string, words: string) => make(state(words)),
    fromSnapshot: (bytes: Uint8Array) => make(decode(bytes)),
  } } as never);
  t.after(async () => {
    await forgetWasmWallet();
    restore.reverse().forEach(fn => fn());
  });
  return { db, install };
}

function client(extra: Partial<LwdTransport> = {}) {
  return createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "fixture", tip: async () => 10, blocks: async () => new Uint8Array(), ...extra,
  } as LwdTransport });
}

/** Only the scan RPCs used by these independent lifecycle regressions. */
class ScanFixture extends EventTarget {
  state = state("uninitialized");
  ops: string[] = [];
  stopped = false;
  postMessage(message: { id: number; op: string; mnemonic?: string; snapshot?: ArrayBuffer }) {
    this.ops.push(message.op);
    let data: Record<string, unknown> = {};
    switch (message.op) {
      case "init": data = { mode: "single-thread", threads: 1 }; break;
      case "create": this.state = state(message.mnemonic!); break;
      case "fromSnapshot": this.state = decode(message.snapshot!); break;
      case "resetScan": this.state.scannedHeight = 0; this.state.pending = false; break;
      case "meta": data = { scanned: this.state.scannedHeight, birthday: 1, nextHeight: this.state.scannedHeight + 1, treesReady: true, sinsemillaLive: true, transparentAddress: "tm-fixture" }; break;
      case "snapshotJson": data = { json: JSON.stringify(preview(this.state)) }; break;
      case "toSnapshot": data = { snapshot: encode(this.state).buffer }; break;
      case "persistenceSnapshot": data = { snapshot: encode(this.state).buffer, json: JSON.stringify(preview(this.state)) }; break;
      case "history": case "memoEnhancementTxids": data = { json: "[]" }; break;
      case "transparentAddress": data = { transparentAddress: "tm-fixture" }; break;
    }
    queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: { id: message.id, ...data } })));
  }
  terminate() { this.stopped = true; }
}

class CrashScanFixture extends ScanFixture {
  hold = "";
  held: Parameters<ScanFixture["postMessage"]>[0] | null = null;
  initOptions: unknown;
  listeners = new Map<string, Set<EventListenerOrEventListenerObject>>();
  override addEventListener(type: string, listener: EventListenerOrEventListenerObject | null) {
    if (listener) {
      if (!this.listeners.has(type)) this.listeners.set(type, new Set());
      this.listeners.get(type)!.add(listener);
    }
    super.addEventListener(type, listener);
  }
  override removeEventListener(type: string, listener: EventListenerOrEventListenerObject | null) {
    if (listener) this.listeners.get(type)?.delete(listener);
    super.removeEventListener(type, listener);
  }
  get listenerCount() { return [...this.listeners.values()].reduce((n, listeners) => n + listeners.size, 0); }
  override postMessage(message: Parameters<ScanFixture["postMessage"]>[0]) {
    if (message.op === "init") this.initOptions = message;
    if (message.op === this.hold) { this.held = message; return; }
    super.postMessage(message);
  }
  release() {
    const message = this.held!;
    this.held = null;
    this.hold = "";
    super.postMessage(message);
  }
}

test("scan errors retire active and queued RPCs; late old events cannot retire an explicit replacement", async t => {
  fixture(t);
  const old = new CrashScanFixture();
  const fresh = new CrashScanFixture();
  let spawned = 0;
  await attachScanWorker(old as unknown as Worker, { threads: 1, preferMulticore: false }, () => {
    spawned++;
    return fresh as unknown as Worker;
  });
  const source = workerScanSession()!;
  const late = [...old.listeners.get("error")!][0] as EventListener;
  old.hold = "applyBlob";
  const active = assert.rejects(source.applyBlob(new Uint8Array()), /scan worker applyBlob failed/);
  const queued = [source.history(1), source.scannedHeight(), source.toSnapshot()]
    .map(promise => assert.rejects(promise, /scan worker restarted/));
  await turn();
  assert.ok(old.held);
  old.dispatchEvent(new Event("error"));
  await Promise.all([active, ...queued]);
  assert.equal(scanWorkerReady(), false);
  assert.equal(scanWorkerBusy(), false);
  assert.equal(workerScanSession(), null);
  assert.equal(old.stopped, true);
  assert.equal(old.listenerCount, 0);
  assert.equal(spawned, 0, "failure must not spawn or replay work");
  await recoverScanWorker();
  assert.equal(spawned, 1);
  late(new Event("error")); // Already queued before its lifecycle listener was removed.
  assert.equal(scanWorkerReady(), true);
  assert.equal(fresh.stopped, false);
  await assert.rejects(source.history(1), /scan worker restarted/);
  assert.deepEqual(fresh.ops, ["init"]);
  assert.deepEqual(await workerScanSession()!.history(1), "[]");
});

test("an idle scan messageerror recovers saved bytes once on concurrent explicit wallet calls with the same artifacts", async t => {
  fixture(t);
  const old = new CrashScanFixture();
  const fresh = new CrashScanFixture();
  fresh.hold = "init";
  let spawned = 0;
  const options = { threads: 3, preferMulticore: true, wasmBasePath: "https://fixture.invalid/same-verified-artifacts" };
  await attachScanWorker(old as unknown as Worker, options, () => { spawned++; return fresh as unknown as Worker; });
  const c = client();
  await c.restore("recover-saved", "regtest", 1);
  const second = client();
  await second.tip(); // Both clients cache the lease that is about to fail.
  old.state.scannedHeight = 77; // Deliberately unsaved work must never be replayed.
  old.dispatchEvent(new Event("messageerror"));
  await turn();
  assert.equal(scanWorkerReady(), false);
  assert.equal(old.listenerCount, 0);
  assert.equal(spawned, 0);
  const loading = c.getWallet();
  const concurrent = second.tip();
  for (let i = 0; i < 20 && !fresh.held; i++) await turn();
  assert.equal(fresh.held?.op, "init");
  assert.equal(spawned, 1);
  fresh.release();
  const [wallet, tip] = await Promise.all([loading, concurrent]);
  assert.equal(wallet.scannedHeight, 10);
  assert.equal(tip.scanned, 10);
  assert.deepEqual(fresh.initOptions, { id: 1, op: "init", ...options });
  assert.equal(fresh.ops.filter(op => op === "fromSnapshot").length, 1);
  assert.equal((await c.getWallet()).scannedHeight, 10);
  await c.sync();
  assert.equal(spawned, 1);
  await forgetWasmWallet();
  await assert.rejects(c.getWallet(), /no wasm wallet/);
  assert.equal(spawned, 1, "Forget must not respawn the remembered worker factory");
});

test("a scan RPC timeout retires all work and does not automatically restart snapshot hydration", async t => {
  fixture(t);
  const scanner = new CrashScanFixture();
  let spawned = 0;
  await attachScanWorker(scanner as unknown as Worker, { threads: 1, preferMulticore: false }, () => {
    spawned++;
    return new CrashScanFixture() as unknown as Worker;
  });
  const timers = new Map<ReturnType<typeof setTimeout>, () => void>();
  const schedule = globalThis.setTimeout;
  const unschedule = globalThis.clearTimeout;
  t.mock.method(globalThis, "setTimeout", (fn: () => void, ms?: number) => {
    const timer = schedule(fn, ms);
    if (ms === 180_000) timers.set(timer, fn);
    return timer;
  });
  t.mock.method(globalThis, "clearTimeout", (timer: ReturnType<typeof setTimeout>) => {
    timers.delete(timer); unschedule(timer);
  });
  scanner.hold = "fromSnapshot";
  const source = workerScanSession()!;
  const active = assert.rejects(source.fromSnapshot(encode(state("timeout"))), /timed out/);
  const queued = assert.rejects(source.scannedHeight(), /scan worker restarted/);
  await turn();
  assert.equal(timers.size, 1);
  [...timers.values()][0]();
  await Promise.all([active, queued]);
  assert.equal(timers.size, 0);
  assert.equal(scanner.listenerCount, 0);
  assert.equal(scanWorkerReady(), false);
  assert.equal(spawned, 0);
});

test("Forget during a crashed worker's saved read prevents recovery from spawning", async t => {
  const { db } = fixture(t);
  const scanner = new CrashScanFixture();
  let spawned = 0;
  await attachScanWorker(scanner as unknown as Worker, { threads: 1, preferMulticore: false }, () => {
    spawned++;
    return new CrashScanFixture() as unknown as Worker;
  });
  const c = client();
  await c.restore("forget-recovery", "regtest", 1);
  scanner.dispatchEvent(new Event("error"));
  db.holdOpens = true;
  const loading = c.sync();
  const rejected = assert.rejects(loading, /no wasm wallet|cancelled/);
  await turn();
  assert.equal(db.opens.length, 1);
  db.holdOpens = false;
  await forgetWasmWallet();
  db.opens.shift()!();
  await rejected;
  assert.equal(spawned, 0);
  assert.equal(await readSavedSnapshot(), null);
});

test("reset ownership discards a late proof without broadcast or restoring the pre-reset snapshot", async t => {
  const f = fixture(t);
  const proof = deferred<{ worker: EventTarget; id: number; snapshot: ArrayBuffer }>();
  class Prover extends EventTarget {
    postMessage(msg: { id: number; kind: string; snapshot: ArrayBuffer }) {
      if (msg.kind === "send") proof.resolve({ worker: this, ...msg });
      else queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: { id: msg.id, ready: true } })));
    }
    terminate() {}
  }
  f.install("window", { addEventListener() {} });
  f.install("document", { addEventListener() {} });
  f.install("Worker", Prover);
  const scanner = new ScanFixture();
  await attachScanWorker(scanner as unknown as Worker, { threads: 1, preferMulticore: false });
  let submits = 0;
  const c = client({ submit: async () => { submits++; return "unused"; } });
  await c.restore("proof-fixture", "regtest", 1);
  const sending = c.send("recipient", "0.00005");
  const rejected = assert.rejects(sending, /cancelled/);
  const pending = await proof.promise;
  await c.resetScan!();
  assert.equal(scanner.state.scannedHeight, 0);
  const before = structuredClone(f.db.records);
  const old = decode(pending.snapshot);
  pending.worker.dispatchEvent(new MessageEvent("message", { data: {
    id: pending.id, hex: "never-broadcast", snapshot: encode({ ...old, pending: true }),
  } }));
  await rejected;
  assert.equal(submits, 0);
  assert.equal(scanner.ops.filter(op => op === "fromSnapshot").length, 0);
  assert.equal((await c.getWallet()).scannedHeight, 0);
  assert.deepEqual(f.db.records, before);
  pending.worker.dispatchEvent(new Event("error")); // Retire only this test's proving worker.
});

test("unlock policy is application-owned and changing it never persists a security override", async t => {
  const { db } = fixture(t);
  const c = client();
  await c.restore("policy-old", "regtest", 1);
  const writes = db.writes.length;
  await c.setUnlockPolicy("each-spend");
  assert.equal(db.writes.length, writes);
  assert.equal(db.records.has("meta"), false);
  assert.equal(c.hasSpendingSeed(), false);
  await forgetWasmWallet();
  await c.restore("policy-new", "regtest", 1);
  assert.equal(c.unlockPolicy(), "each-spend");
  assert.equal(c.hasSpendingSeed(), false);
});

test("a new client refuses an existing worker whose wallet generation differs from saved bytes", async t => {
  fixture(t);
  const scanner = new ScanFixture();
  await attachScanWorker(scanner as unknown as Worker, { threads: 1, preferMulticore: false });
  const a = client();
  await a.restore("worker-old", "regtest", 1);
  const next = state("saved-by-other-tab");
  const generation = await advanceWalletGeneration();
  await saveWalletSnapshot(encode(next), preview(next) as never, () => true, generation);
  const fromSnapshot = scanner.ops.filter(op => op === "fromSnapshot").length;
  const b = client();
  await assert.rejects(b.tip(), /wallet changed in another tab/);
  assert.equal(scanner.state.unifiedAddress, "worker-old");
  assert.equal(scanner.ops.filter(op => op === "fromSnapshot").length, fromSnapshot);
  assert.equal(decode((await readSavedSnapshot())!).unifiedAddress, "saved-by-other-tab");
});

for (const method of ["utxos", "mempool"] as const) {
  test(`a deferred ${method} response cannot apply to a replacement worker wallet`, async t => {
    const { db } = fixture(t);
    const scanner = new ScanFixture();
    await attachScanWorker(scanner as unknown as Worker, { threads: 1, preferMulticore: false });
    const entered = deferred<void>();
    const response = deferred<any[]>();
    const c = client({ [method]: async () => { entered.resolve(); return response.promise; } });
    await c.restore("response-old", "regtest", 1);
    const syncing = c.sync();
    const rejected = assert.rejects(syncing, /cancelled/);
    await entered.promise;
    await c.restore("response-new", "regtest", 1);
    const before = structuredClone(db.records);
    const op = method === "utxos" ? "applyUtxos" : "applyMempool";
    const calls = scanner.ops.filter(value => value === op).length;
    response.resolve(method === "utxos" ? [{ txid: "old-response" }] : ["old-response"]);
    await rejected;
    assert.equal(scanner.ops.filter(value => value === op).length, calls);
    assert.equal((await c.getWallet()).unifiedAddress, "response-new");
    assert.deepEqual(db.records, before);
  });
}

test("scan worker falls back to verified ST when the MT artifact lacks its thread-pool ABI", async t => {
  const f = fixture(t);
  const calls: string[] = [];
  f.install("__lifecycleReviewArtifactCalls", calls);
  t.mock.method(console, "warn", () => {});
  const reply = deferred<Record<string, unknown>>();
  const scope = { onmessage: null as ((event: MessageEvent) => void) | null,
    postMessage: (message: Record<string, unknown>) => reply.resolve(message) };
  f.install("self", scope);
  const moduleUrl = (code: string) => `data:text/javascript;base64,${Buffer.from(code).toString("base64")}`;
  const mark = "globalThis.__lifecycleReviewArtifactCalls";
  const mt = moduleUrl(`export default async function(){${mark}.push('init-mt')}`);
  const st = moduleUrl(`export default async function(){${mark}.push('init-st')}
    export const WasmWallet={capabilities:()=>'{"threads":1}'};`);
  const integrity = moduleUrl(`export const allowMissingBuiltWasm = () => false;
    export async function verifyWasmAt(url){${mark}.push(url);return new ArrayBuffer(1)}`);
  // Execute the real worker entry point with only its external artifact modules
  // replaced. No generated artifacts or production files are changed by this test.
  const url = new URL("../src/scan.worker.ts", import.meta.url);
  const ts = createRequire(import.meta.url)("typescript") as typeof import("typescript");
  const hardwareOps = moduleUrl(ts.transpileModule(readFileSync(new URL("../src/hardware-ops.ts", import.meta.url), "utf8"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  }).outputText);
  const source = readFileSync(url, "utf8")
    .replace('from "./integrity"', `from ${JSON.stringify(integrity)}`)
    .replace('from "./hardware-ops"', `from ${JSON.stringify(hardwareOps)}`)
    .replace('import("./generated-mt/z_wasm.js")', `import(${JSON.stringify(mt)})`)
    .replace('import("./generated/z_wasm.js")', `import(${JSON.stringify(st)})`)
    .replaceAll("import.meta.url", JSON.stringify(url.href));
  const compiled = ts.transpileModule(source, { compilerOptions: {
    target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext,
  } }).outputText;
  await import(moduleUrl(compiled));
  scope.onmessage!(new MessageEvent("message", { data: { id: 1, op: "init", preferMulticore: true, threads: 4 } }));
  const result = await reply.promise;
  assert.equal(result.error, undefined);
  assert.equal(result.mode, "single-thread");
  assert.equal(result.multicore, false);
  assert.equal(result.threads, 1);
  assert.deepEqual(calls.map(value => value.startsWith("file:") ? value.split("/src/")[1] : value), [
    "generated-mt/z_wasm_bg.wasm", "init-mt", "generated/z_wasm_bg.wasm", "init-st",
  ]);
});
