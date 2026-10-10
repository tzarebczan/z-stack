import assert from "node:assert/strict";
import { test } from "node:test";
import { existsSync, readFileSync } from "node:fs";
import { setImmediate as turn } from "node:timers/promises";
import { coalescedTask } from "../src/coalesced-task.ts";
import { sha256Hex } from "../src/integrity.ts";
import { attachScanWorker, workerScanSession, localScanSession, restartScanWorker } from "../src/scan-host.ts";
import { createWasmClient, cancelWasmSync, prewarmProveWorker, prewarmOrchardProvingKey, orchardProvingKeyReady, configureWasmWorkerBasePath } from "../src/wasm-client.ts";
import { memoryWalletStorage } from "../src/storage.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";
import { REGTEST_FAUCET_MNEMONIC } from "../src/constants.ts";
import { readSavedSnapshotRecord } from "../src/snapshot-storage.ts";
import { saveWalletSnapshot } from "../src/snapshot-storage.ts";
import type { WalletSnapshot } from "@z-stack/core";

test("snapshot backpressure retains one active and only the latest pending save", async () => {
  const save = coalescedTask();
  const writes: number[] = [];
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => { release = resolve; });
  const first = save(async () => { writes.push(0); await blocked; });
  await turn();
  const requests = Array.from({ length: 50 }, (_, i) => save(async () => { writes.push(i + 1); }));
  assert.deepEqual(writes, [0]);
  release();
  await Promise.all([first, ...requests]);
  assert.deepEqual(writes, [0, 50]);
});

test("failed snapshot writes do not poison the next save", async () => {
  const save = coalescedTask();
  const first = save(async () => { throw new Error("quota exceeded"); });
  let saved = false;
  const second = save(async () => { saved = true; });
  await assert.rejects(first, /quota exceeded/);
  await second;
  assert.equal(saved, true);
});

test("initialize instantiates verified bytes once across concurrent and repeated calls", async (t) => {
  const artifact = new URL("../src/generated/z_wasm_bg.wasm", import.meta.url);
  if (!existsSync(artifact)) { t.skip("run pnpm build:wasm first"); return; }
  const bytes = readFileSync(artifact);
  const sha256 = await sha256Hex(bytes);
  const requests: string[] = [];
  t.mock.method(globalThis, "fetch", async (url: string) => {
    requests.push(String(url));
    return url.endsWith("integrity.json") ? Response.json({ sha256 }) : new Response(bytes);
  });
  const { initialize, cryptoSmoke } = await import("../src/lab.ts");
  const options = { wasmBasePath: "https://example.invalid/wasm", preferMulticore: false, prewarmProvingKey: false, prewarmProveWorker: false };
  await Promise.all([initialize(options), initialize(options), initialize(options)]);
  await initialize(options);
  assert.equal(cryptoSmoke(), "zakura-orchard linked");
  assert.deepEqual(requests, ["https://example.invalid/wasm/integrity.json", "https://example.invalid/wasm/z_wasm_bg.wasm"]);
});

test("auto-sync retries unavailable ranges and selective memos without waiting for a new block", async (t) => {
  const db = memoryIndexedDb();
  const oldDb = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const oldRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { value: db.indexedDB, configurable: true });
  Object.defineProperty(globalThis, "IDBKeyRange", { value: db.IDBKeyRange, configurable: true });
  t.after(() => {
    if (oldDb) Object.defineProperty(globalThis, "indexedDB", oldDb);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (oldRange) Object.defineProperty(globalThis, "IDBKeyRange", oldRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  await saveWalletSnapshot(new Uint8Array([1]), {
    network: "regtest", server: "", birthdayHeight: 1, scannedHeight: 10,
    unifiedAddress: "uregtest-fixture", transparentAddress: null,
    zip321: "zcash:uregtest-fixture", balance: { totalAvailable: 0 },
  } as WalletSnapshot, () => true);
  let tick!: () => void;
  t.mock.method(globalThis, "setInterval", (fn) => { tick = fn as () => void; return 1 as never; });
  t.mock.method(globalThis, "clearInterval", () => {});
  const { createWallet } = await import("../src/create-wallet.ts");
  const wallet = await createWallet({ unlockPolicy: "session", autoShield: true, network: "regtest", server: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
  }, autoShield: false });
  t.after(() => wallet.close());
  t.mock.method(wallet, "tip", async () => ({ tip: 10, scanned: 10, behind: 0 }));
  const snapshot = { network: "regtest", scannedHeight: 10,
    transparentScanStatus: "complete", sharedMemoStatus: "complete" } as WalletSnapshot;
  t.mock.method(wallet, "getWallet", async () => snapshot);
  let synced = 0;
  t.mock.method(wallet, "sync", async () => { synced++; return snapshot; });
  wallet.startAutoSync();
  const runTick = async () => { tick(); for (let i = 0; i < 10; i++) await turn(); };
  for (const feature of ["transparentScanStatus", "sharedMemoStatus", "memoFetchStatus"] as const) {
    wallet.setMemoFetch(feature === "sharedMemoStatus" ? "shared" : feature === "memoFetchStatus" ? "auto" : "on-demand");
    snapshot[feature] = "unavailable";
    const before = synced;
    await runTick();
    assert.equal(synced, before + 1, `${feature} retries at the same chain tip`);
    snapshot[feature] = "unsupported";
    await runTick();
    assert.equal(synced, before + 1, "unsupported servers do not cause retry loops");
    snapshot[feature] = "complete";
  }
});

// A restore at the mock scan height stays inside the default sync window.
const WORKER_BIRTHDAY = 200_000;

class ScanWorker extends EventTarget {
  ops: string[] = [];
  scanned = WORKER_BIRTHDAY;
  transparentAvailable = 0;
  transparentScanHeight = 0;
  snapshotHeights: number[] = [];
  applyFromPayload = false;
  onApplied?: () => void;
  blockedSnapshot: { id: number } | null = null;
  holdSnapshot = false;
  snapshot = new Uint8Array([1, 2, 3]);
  pendingRaw: string[] = [];
  postMessage(msg: { id: number; op: string; blob?: ArrayBuffer }) {
    this.ops.push(msg.op);
    if (msg.op === "persistenceSnapshot" && this.holdSnapshot) { this.blockedSnapshot = msg; return; }
    let data: Record<string, unknown> = {};
    switch (msg.op) {
      case "init": data = { threads: 1, mode: "single-thread" }; break;
      case "meta": data = { scanned: this.scanned, birthday: 1, nextHeight: this.scanned + 1,
        treesReady: true, sinsemillaLive: true, transparentAddress: "fixture-taddr", transparentCompact: true }; break;
      case "snapshotJson": data = { json: JSON.stringify({ network: "regtest", birthdayHeight: 1, scannedHeight: this.scanned, transparentScanHeight: this.transparentScanHeight, balance: { totalAvailable: this.transparentAvailable, transparentAvailable: this.transparentAvailable } }) }; break;
      case "applyBlob":
        this.scanned = this.applyFromPayload && msg.blob
          ? new DataView(msg.blob).getUint32(0, false)
          : this.scanned + 1;
        this.onApplied?.();
        data = { scanned: this.scanned };
        break;
      case "toSnapshot": data = { snapshot: this.snapshot.buffer }; break;
      case "pendingRawTxs": data = { json: JSON.stringify(this.pendingRaw) }; break;
      case "persistenceSnapshot":
        this.snapshotHeights.push(this.scanned);
        data = { snapshot: this.snapshot.buffer,
          json: JSON.stringify({ network: "regtest", unifiedAddress: "uregtest-fixture", birthdayHeight: 1,
            scannedHeight: this.scanned, balance: { totalAvailable: 0 } }) };
        break;
    }
    queueMicrotask(() => this.reply(msg.id, data));
  }
  reply(id: number, data: Record<string, unknown>) { this.dispatchEvent(new MessageEvent("message", { data: { id, ...data } })); }
  terminate() {}
}

test("runtime follows the active scan worker and falls back after retirement", async () => {
  const { wasmRuntime } = await import("../src/runtime.ts");
  const initial = wasmRuntime();
  const worker = new ScanWorker();
  const reply = worker.reply.bind(worker);
  worker.reply = (id, data) => reply(id, "mode" in data
    ? { ...data, mode: "multi-thread", threads: 2 } : data);
  await attachScanWorker(worker as unknown as Worker, { threads: 2, preferMulticore: true });
  assert.equal(wasmRuntime()?.mode, "multi-thread");
  assert.equal(wasmRuntime()?.threads, 2);
  assert.equal(wasmRuntime()?.scanWorker, true);
  await restartScanWorker();
  assert.deepEqual(wasmRuntime(), initial);
});

test("repeated and concurrent catch-up reuses the worker wallet and one download", async () => {
  const worker = new ScanWorker();
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  assert.equal(workerScanSession(), workerScanSession());
  assert.equal((await workerScanSession()!.toSnapshot()).buffer, worker.snapshot.buffer);
  let tip = worker.scanned;
  let downloads = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => tip,
    blocks: async () => { downloads++; await turn(); return new Uint8Array([1]); },
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.ops.length = 0;
  tip++;
  // Birthday is over 150k blocks ago; only one block is actually unscanned.
  await Promise.all([client.sync(), client.sync(), client.sync()]);
  const after = await client.sync();
  assert.equal(downloads, 1);
  assert.equal(worker.ops.filter((op) => op === "applyBlob").length, 1);
  assert.equal(worker.ops.filter((op) => op === "toSnapshot" || op === "fromSnapshot").length, 0);
  assert.equal(after.scannedHeight, tip);
  cancelWasmSync();
});

test("a compact-range outage resumes from the in-memory scanned height without replacing the worker", async (t) => {
  const worker = new ScanWorker();
  worker.applyFromPayload = true;
  await restartScanWorker(() => worker as unknown as Worker);
  t.after(() => cancelWasmSync());
  const origin = worker.scanned;
  const tip = origin + 4_001;
  let releaseFirst!: () => void;
  const firstApplied = new Promise<void>(resolve => { releaseFirst = resolve; });
  worker.onApplied = releaseFirst;
  const starts: number[] = [];
  const headings: string[] = [];
  let secondAttempts = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => tip,
    blocks: async (start, end) => {
      starts.push(start);
      if (start === origin + 4_001 && ++secondAttempts === 1) {
        await firstApplied;
        throw new TypeError("local light server restarting");
      }
      const blob = new Uint8Array(4);
      new DataView(blob.buffer).setUint32(0, end, false);
      return blob;
    },
  } }, (progress) => { if (progress.heading) headings.push(progress.heading); });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.ops.length = 0;
  const result = await client.sync();
  assert.equal(result.scannedHeight, tip);
  assert.deepEqual(starts, [origin + 1, origin + 4_001, origin + 4_001]);
  assert.equal(worker.ops.filter(op => op === "applyBlob").length, 2, "the successful first range is not replayed");
  assert.ok(headings.includes("Waiting for light server"));
  assert.equal(workerScanSession()?.kind, "worker");
});

test("an expensive WASM apply does not report a dead light server", async (t) => {
  const worker = new ScanWorker();
  worker.applyFromPayload = true;
  await restartScanWorker(() => worker as unknown as Worker);
  t.after(() => cancelWasmSync());
  let clock = 0;
  t.mock.method(Date, "now", () => clock);
  worker.onApplied = () => { clock += 20_000; };
  const origin = worker.scanned;
  const headings: string[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => origin + 8_000,
    blocks: async (_start, end) => {
      const blob = new Uint8Array(4);
      new DataView(blob.buffer).setUint32(0, end, false);
      return blob;
    },
  } }, (progress) => { if (progress.heading) headings.push(progress.heading); });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  await client.sync();
  assert.equal(worker.scanned, origin + 8_000);
  assert.equal(headings.includes("Download stuck"), false,
    "the active 20s apply cannot be mistaken for an unreachable light server");
  assert.ok(headings.includes("Scanning compact blocks"));
});

test("a failed block apply reloads the saved snapshot on the next sync instead of failing forever", async (t) => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const db = memoryIndexedDb();
  const priorIdb = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const priorRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: db.indexedDB });
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: db.IDBKeyRange });
  t.after(() => {
    cancelWasmSync();
    if (priorIdb) Object.defineProperty(globalThis, "indexedDB", priorIdb);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (priorRange) Object.defineProperty(globalThis, "IDBKeyRange", priorRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  // The engine refuses every later apply and save once one block failed.
  let poisoned = false;
  let failNext = false;
  const post = worker.postMessage.bind(worker);
  worker.postMessage = (msg) => {
    if (msg.op === "fromSnapshot") poisoned = false;
    if (msg.op === "applyBlob" && (failNext || poisoned)) {
      worker.ops.push(msg.op);
      const error = poisoned ? "scan state is invalid after: expected compact block 200002, got 200005"
        : "expected compact block 200002, got 200005";
      failNext = false;
      poisoned = true;
      queueMicrotask(() => worker.reply(msg.id, { error }));
      return;
    }
    post(msg);
  };
  let tip = worker.scanned;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => tip,
    blocks: async () => new Uint8Array([1]),
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.ops.length = 0;
  tip++;
  failNext = true;
  await assert.rejects(client.sync(), /expected compact block/);
  const after = await client.sync();
  assert.ok(worker.ops.includes("fromSnapshot"), "the next sync hydrates the last saved snapshot");
  assert.equal(after.scannedHeight, tip);
});

test("each sync resubmits saved unmined sends and a node that already has one is not an error", async (t) => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  t.after(() => cancelWasmSync());
  const submitted: string[] = [];
  let tip = worker.scanned;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => tip,
    blocks: async () => new Uint8Array([1]),
    submit: async (hex: string) => {
      submitted.push(hex);
      if (hex === "bb") throw new Error("transaction is already in the mempool");
      return "txid";
    },
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.pendingRaw = ["aa", "bb"];
  tip++;
  await client.sync();
  assert.deepEqual(submitted, ["aa", "bb"]);
  worker.pendingRaw = [];
  await client.sync();
  assert.deepEqual(submitted, ["aa", "bb"], "nothing pending, nothing sent");
});

test("a mistyped recovery phrase is rejected before it can fence out the saved wallet", async (t) => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const db = memoryIndexedDb();
  const priorIdb = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const priorRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: db.indexedDB });
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: db.IDBKeyRange });
  t.after(() => {
    cancelWasmSync();
    if (priorIdb) Object.defineProperty(globalThis, "indexedDB", priorIdb);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (priorRange) Object.defineProperty(globalThis, "IDBKeyRange", priorRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => worker.scanned,
    blocks: async () => new Uint8Array([1]),
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  const saved = await readSavedSnapshotRecord();
  assert.ok(saved);
  worker.ops.length = 0;
  const typo = REGTEST_FAUCET_MNEMONIC.replace(/\S+$/, "notaword");
  await assert.rejects(client.restore(typo, "regtest", 1), /invalid recovery phrase/);
  assert.deepEqual(worker.ops, [], "the worker's wallet was not touched");
  assert.deepEqual((await readSavedSnapshotRecord())?.generation, saved.generation,
    "the saved wallet keeps its generation, so a reload still finds it");
});

test("a slow IndexedDB checkpoint queues one latest-height follow-up after commit", async (t) => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const db = memoryIndexedDb();
  const priorIdb = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const priorRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: db.indexedDB });
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: db.IDBKeyRange });
  t.after(() => {
    cancelWasmSync();
    if (priorIdb) Object.defineProperty(globalThis, "indexedDB", priorIdb);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (priorRange) Object.defineProperty(globalThis, "IDBKeyRange", priorRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 328_000,
    blocks: async () => new Uint8Array([1]),
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.ops.length = 0;
  worker.snapshotHeights.length = 0;
  db.holdOpens = true;
  const sync = client.sync();
  for (let i = 0; i < 200 && worker.ops.filter(op => op === "applyBlob").length < 32; i++) await turn();
  assert.equal(worker.ops.filter(op => op === "applyBlob").length, 32);
  assert.equal(worker.ops.filter(op => op === "persistenceSnapshot").length, 1,
    "the 32nd page must not queue another capture while the first commit is waiting");
  assert.equal(db.opens.length, 1, "the first checkpoint is waiting to open IndexedDB");
  db.holdOpens = false;
  db.opens.shift()!();
  await sync;
  assert.deepEqual(worker.snapshotHeights.slice(0, 2), [200_016, 200_032],
    "a follow-up capture takes the latest scanned height after the slow commit");
  assert.equal(worker.ops.filter(op => op === "persistenceSnapshot").length, 2,
    "one latest-height follow-up commits; unchanged final checkpoints do not rewrite it");
});

test("cancelling a scan fences a pending follow-up checkpoint", async (t) => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const db = memoryIndexedDb();
  const priorIdb = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const priorRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: db.indexedDB });
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: db.IDBKeyRange });
  t.after(() => {
    cancelWasmSync();
    if (priorIdb) Object.defineProperty(globalThis, "indexedDB", priorIdb);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (priorRange) Object.defineProperty(globalThis, "IDBKeyRange", priorRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 328_000,
    blocks: async () => new Uint8Array([1]),
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.snapshotHeights.length = 0;
  db.holdOpens = true;
  const syncing = client.sync();
  for (let i = 0; i < 200 && worker.ops.filter(op => op === "applyBlob").length < 32; i++) await turn();
  assert.equal(worker.snapshotHeights.length, 1);
  cancelWasmSync();
  db.holdOpens = false;
  db.opens.shift()?.();
  await assert.rejects(syncing, /cancelled/);
  await turn();
  assert.equal(worker.snapshotHeights.length, 1, "the retired scan cannot start another capture");
});

test("post-scan UTXO and mempool reads overlap but apply in wallet order", async () => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  let releaseUtxos!: (rows: []) => void;
  let releaseMempool!: (rows: Array<{ hex: string }>) => void;
  const utxos = new Promise<[]>(resolve => { releaseUtxos = resolve; });
  const mempool = new Promise<Array<{ hex: string }>>(resolve => { releaseMempool = resolve; });
  const reads: string[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => worker.scanned,
    blocks: async () => new Uint8Array(),
    utxos: async () => { reads.push("utxos"); return utxos; },
    mempool: async () => { reads.push("mempool"); return mempool; },
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  worker.ops.length = 0;
  const sync = client.sync();
  for (let i = 0; i < 20 && reads.length < 2; i++) await turn();
  assert.deepEqual(reads, ["utxos", "mempool"]);
  releaseMempool([{ hex: "00" }]);
  await turn();
  assert.equal(worker.ops.includes("applyMempool"), false, "mempool must wait for the UTXO read");
  releaseUtxos([]);
  await sync;
  assert.ok(worker.ops.indexOf("applyUtxos") >= 0);
  assert.ok(worker.ops.indexOf("applyMempool") > worker.ops.indexOf("applyUtxos"));
  cancelWasmSync();
});

test("cancellation fences snapshots during serialization and during IndexedDB open", async (t) => {
  const worker = new ScanWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const db = memoryIndexedDb();
  const writes = () => db.writes.filter((key) => key === "default").length;
  const previous = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const previousRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: db.indexedDB });
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: db.IDBKeyRange });
  t.after(() => {
    if (previous) Object.defineProperty(globalThis, "indexedDB", previous);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (previousRange) Object.defineProperty(globalThis, "IDBKeyRange", previousRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => worker.scanned, blocks: async () => new Uint8Array(),
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  assert.equal(writes(), 1);
  worker.holdSnapshot = true;
  const serializing = client.nextAddress();
  await turn();
  assert.ok(worker.blockedSnapshot);
  cancelWasmSync();
  worker.reply(worker.blockedSnapshot.id, { snapshot: worker.snapshot.buffer, json: "{}" });
  await serializing;
  assert.equal(writes(), 1, "canceled serialization must not write");
  worker.holdSnapshot = false;
  db.holdOpens = true;
  const saving = client.nextAddress();
  await turn();
  assert.equal(db.opens.length, 1);
  cancelWasmSync();
  db.opens.shift()!();
  await saving;
  assert.equal(writes(), 1, "cancellation during DB open must not resurrect a snapshot");
  assert.ok(db.closes >= 3, "IDB handles close on success and cancellation");
});

test("local wallet replacement and forget release Rust allocations exactly once", async () => {
  let freed = 0;
  const handle = () => ({ free: () => { freed++; }, scannedHeight: () => 7 });
  const session = localScanSession(handle() as never, () => handle() as never);
  await session.fromSnapshot(new Uint8Array());
  assert.equal(freed, 1);
  await session.forget();
  await session.forget();
  assert.equal(freed, 2);
  await assert.rejects(session.scannedHeight(), /no wasm wallet/);
});

test("worker post failures and crashes settle RPCs and release listeners", async () => {
  class FailingWorker extends ScanWorker {
    fail = "";
    listeners = 0;
    override addEventListener(...args: Parameters<EventTarget["addEventListener"]>) {
      this.listeners++;
      super.addEventListener(...args);
    }
    override removeEventListener(...args: Parameters<EventTarget["removeEventListener"]>) {
      this.listeners--;
      super.removeEventListener(...args);
    }
    override postMessage(msg: { id: number; op: string }) {
      if (this.fail === "post") throw new Error("cannot post");
      if (this.fail === "crash") { queueMicrotask(() => this.dispatchEvent(new Event("error"))); return; }
      super.postMessage(msg);
    }
  }
  const worker = new FailingWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const oldSession = workerScanSession()!;
  worker.fail = "post";
  await assert.rejects(oldSession.scannedHeight(), /cannot post/);
  assert.equal(worker.listeners, 0);
  assert.equal(workerScanSession(), null);
  await assert.rejects(oldSession.history(1), /scan worker restarted/);
  worker.fail = "";
  await restartScanWorker(() => worker as unknown as Worker);
  assert.notEqual(workerScanSession(), oldSession);
  assert.equal(await workerScanSession()!.scannedHeight(), worker.scanned);
  worker.fail = "crash";
  // applyBlob intentionally has no timeout: an error event must still release it.
  await assert.rejects(workerScanSession()!.applyBlob(new Uint8Array()), /scan worker applyBlob failed/);
  assert.equal(worker.listeners, 0);
});

test("proving worker lifecycle recovers only on explicit calls and preserves its artifact source", async (t) => {
  const scan = new ScanWorker();
  await restartScanWorker(() => scan as unknown as Worker);
  let broadcasts = 0;
  let tipLag = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => scan.scanned + tipLag, blocks: async () => new Uint8Array(),
    submit: async () => { broadcasts++; return "fixture-txid"; },
  } });
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  const base = "https://fixture.invalid/verified-artifacts";
  configureWasmWorkerBasePath(base);
  type Request = { id: number; kind: string; wasmBasePath?: string };
  const workers: ProveWorker[] = [];
  let nextMode: "reply" | "hold" = "reply";
  class ProveWorker extends EventTarget {
    messages: Request[] = [];
    mode: "reply" | "hold" | "throw" = nextMode;
    terminated = 0;
    listeners = new Map<string, Set<EventListenerOrEventListenerObject>>();
    constructor() { super(); workers.push(this); }
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
    get listenerCount() { return [...this.listeners.values()].reduce((sum, set) => sum + set.size, 0); }
    postMessage(msg: Request) {
      this.messages.push(msg);
      if (this.mode === "throw") throw new Error("cannot post proof");
      if (this.mode === "hold") return;
      queueMicrotask(() => this.reply(msg));
    }
    reply(msg: Request, error?: string) {
      this.dispatchEvent(new MessageEvent("message", { data: {
        id: msg.id, ready: msg.kind === "warm", hex: "fixture-proved-transaction", txid: "ab".repeat(32), snapshot: new Uint8Array([1, 2, 3]), error,
      } }));
    }
    terminate() { this.terminated++; }
    crash(kind = "error") { this.dispatchEvent(new Event(kind)); }
  }
  const bindings = (globalThis as any).__zStackWasm;
  for (const [key, value] of Object.entries({ window: new EventTarget(), document: new EventTarget(),
    Worker: ProveWorker, __zStackWasm: { ...bindings, orchardProvingKeyReady: () => true } })) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, value });
    t.after(() => {
      if (previous) Object.defineProperty(globalThis, key, previous);
      else Reflect.deleteProperty(globalThis, key);
    });
  }
  t.mock.method(console, "warn", () => {});
  const timers = new Map<ReturnType<typeof setTimeout>, () => void>();
  const schedule = globalThis.setTimeout;
  const unschedule = globalThis.clearTimeout;
  t.mock.method(globalThis, "setTimeout", (callback: () => void, ms?: number) => {
    const timer = schedule(callback, ms);
    if (ms === 600_000) timers.set(timer, callback);
    return timer;
  });
  t.mock.method(globalThis, "clearTimeout", (timer: ReturnType<typeof setTimeout>) => {
    timers.delete(timer); unschedule(timer);
  });
  t.after(() => { workers.at(-1)?.crash(); cancelWasmSync(); });
  const current = () => workers.at(-1)!;
  const kinds = (worker = current()) => worker.messages.map((msg) => msg.kind);
  const assertRetired = (worker: ProveWorker) => {
    assert.equal(worker.terminated, 1);
    assert.equal(worker.listenerCount, 0, "retired workers release all RPC and lifecycle listeners");
  };

  await t.test("preload is shared, warm state is cleared even by an idle crash", async () => {
    prewarmProveWorker(); prewarmProveWorker();
    await turn();
    assert.deepEqual(kinds(), ["init"]);
    assert.equal(orchardProvingKeyReady(), false, "a key in the UI instance is not a worker key");
    assert.equal((await prewarmOrchardProvingKey()).ready, true);
    assert.equal(orchardProvingKeyReady(), true);
    assert.deepEqual(kinds(), ["init", "warm"]);
    await prewarmOrchardProvingKey();
    assert.deepEqual(kinds(), ["init", "warm"], "a ready key does not enqueue another build");
    const worker = current();
    worker.crash();
    assertRetired(worker);
    assert.equal(orchardProvingKeyReady(), false);
    assert.equal(timers.size, 0);
    await turn();
    assert.equal(workers.length, 1, "crashes never spawn or replay work automatically");
    assert.throws(() => configureWasmWorkerBasePath(`${base}-different`), /cannot change wasmBasePath/);
    configureWasmWorkerBasePath(base);
  });

  await t.test("all concurrent RPCs settle and stale failure cleanup cannot reset a replacement", async () => {
    nextMode = "hold";
    prewarmProveWorker();
    const worker = current();
    const lateError = [...worker.listeners.get("error")!][0] as EventListener;
    const warm1 = prewarmOrchardProvingKey();
    const warm2 = prewarmOrchardProvingKey();
    const rejectedSend = assert.rejects(client.send("uregtest-fixture", "0.00005"), /prove worker failed/);
    await turn();
    assert.deepEqual(kinds(), ["init", "warm", "send"], "concurrent warm requests share one job");
    assert.equal(timers.size, 3);
    worker.crash("messageerror");
    assertRetired(worker);
    assert.equal(timers.size, 0);
    nextMode = "reply";
    prewarmProveWorker(); // Before the old preload rejection's microtask runs.
    const replacement = current();
    assert.notEqual(replacement, worker);
    await Promise.all([rejectedSend, warm1, warm2]);
    assert.equal((await warm1).ready, false);
    assert.equal((await warm2).ready, false);
    await turn();
    prewarmProveWorker();
    assert.deepEqual(kinds(replacement), ["init"], "old catch must not clear the new preload promise");
    assert.equal((await prewarmOrchardProvingKey()).ready, true);
    lateError(new Event("error"));
    for (const msg of worker.messages) worker.reply(msg);
    await turn();
    assert.equal(orchardProvingKeyReady(), true);
    assert.equal(replacement.terminated, 0);
    assert.equal(broadcasts, 0, "failed proofs cannot broadcast or replay");
    assert.equal(timers.size, 0);
    assert.ok(workers.every((w) => w.messages.every((msg) => msg.wasmBasePath === base)));
  });

  await t.test("a resolved warm reply cannot mark a retired worker ready", async () => {
    current().crash();
    nextMode = "hold";
    prewarmProveWorker();
    const worker = current();
    worker.reply(worker.messages[0]);
    await turn();
    const warming = prewarmOrchardProvingKey();
    worker.reply(worker.messages.at(-1)!);
    worker.crash();
    nextMode = "hold";
    prewarmProveWorker();
    assert.equal((await warming).ready, false);
    assert.equal(orchardProvingKeyReady(), false);
    const replacement = current();
    replacement.reply(replacement.messages[0]);
    await turn();
    assertRetired(worker);
    assert.equal(timers.size, 0);
  });

  await t.test("postMessage failure retires the session and rejects other queued work", async () => {
    const worker = current();
    const warming = prewarmOrchardProvingKey();
    worker.mode = "throw";
    await assert.rejects(client.send("uregtest-fixture", "0.00005"), /cannot post proof/);
    assert.equal((await warming).ready, false);
    assertRetired(worker);
    assert.equal(timers.size, 0);
    assert.equal(broadcasts, 0);
  });

  await t.test("timeout retires all queued jobs, and an explicit send uses a fresh worker", async () => {
    nextMode = "hold";
    prewarmProveWorker();
    const worker = current();
    const warming = prewarmOrchardProvingKey();
    const rejectedSend = assert.rejects(client.send("uregtest-fixture", "0.00005"), /prove worker timed out/);
    await turn();
    assert.equal(timers.size, 3);
    [...timers.values()][0]();
    await rejectedSend;
    assert.equal((await warming).ready, false);
    assertRetired(worker);
    assert.equal(timers.size, 0);
    assert.equal(broadcasts, 0);
    nextMode = "reply";
    await client.send("uregtest-fixture", "0.00005");
    assert.notEqual(current(), worker);
    assert.deepEqual(kinds(), ["send"], "fresh worker initializes itself for the explicit proof request");
    assert.equal(current().messages[0].wasmBasePath, base);
    assert.equal(broadcasts, 1);
    assert.equal(timers.size, 0);
  });

  await t.test("a wallet far behind the tip refuses to prove a transaction that would expire", async () => {
    const worker = current();
    const before = worker.messages.length;
    tipLag = 50;
    await assert.rejects(client.send("uregtest-fixture", "0.00005"), (e: unknown) =>
      (e as { code?: string }).code === "sync_required" && /sync before sending/.test(String(e)));
    tipLag = 0;
    assert.equal(worker.messages.length, before, "no proof was requested");
  });

  await t.test("two spends started together never both reach the prover", { timeout: 10_000 }, async () => {
    const worker = current();
    const before = worker.messages.length;
    worker.mode = "hold";
    const first = assert.rejects(client.send("uregtest-fixture", "0.00005"), /insufficient funds/);
    const second = client.send("uregtest-fixture", "0.00005");
    await assert.rejects(second, /already in progress/);
    for (let i = 0; i < 50 && worker.messages.length === before; i++) await turn();
    assert.equal(worker.messages.length, before + 1, "only the first spend asked for a proof");
    worker.reply(worker.messages.at(-1)!, "insufficient funds");
    await first;
    worker.mode = "reply";
  });

  await t.test("ordinary proof errors retain the healthy worker and permit explicit retries", async () => {
    const worker = current();
    worker.mode = "hold";
    const rejected = assert.rejects(client.send("uregtest-fixture", "0.00005"), /insufficient funds/);
    await turn();
    worker.reply(worker.messages.at(-1)!, "insufficient funds");
    await rejected;
    assert.equal(worker.terminated, 0);
    assert.equal(timers.size, 0);
    worker.mode = "reply";
    assert.equal((await prewarmOrchardProvingKey()).ready, true);
    assert.equal(current(), worker);
    assert.equal(broadcasts, 1);
  });

  await t.test("failed preload and warm responses can be retried explicitly", async () => {
    current().crash();
    nextMode = "hold";
    prewarmProveWorker();
    const worker = current();
    worker.reply(worker.messages[0], "artifact fetch failed");
    await turn();
    prewarmProveWorker();
    assert.deepEqual(kinds(), ["init", "init"]);
    worker.reply(worker.messages[1]);
    await turn();
    const warming = prewarmOrchardProvingKey();
    worker.reply(worker.messages.at(-1)!, "key initialization failed");
    assert.equal((await warming).ready, false);
    assert.equal(orchardProvingKeyReady(), false);
    worker.mode = "reply";
    assert.equal((await prewarmOrchardProvingKey()).ready, true);
    assert.equal(worker.terminated, 0);
    assert.equal(timers.size, 0);
    assert.equal(broadcasts, 1);
  });

  await t.test("close interrupts a stalled stateless proof and an explicit reopen uses a fresh actor", async () => {
    const old = current(); old.mode = "hold";
    const before = old.messages.length, priorBroadcasts = broadcasts;
    const rejected = assert.rejects(client.send("uregtest-fixture", "0.00005"), /cancelled/);
    for (let i = 0; i < 20 && old.messages.length === before; i++) await turn();
    assert.equal(old.messages.length, before + 1);
    const late = old.messages.at(-1)!;
    await client.dispose(); await rejected;
    assertRetired(old); assert.equal(timers.size, 0); assert.equal(broadcasts, priorBroadcasts);
    const reopenedScan = new ScanWorker();
    await restartScanWorker(() => reopenedScan as unknown as Worker);
    const reopened = createWasmClient({ network: "regtest", unlockPolicy: "session", autoShield: false,
      transport: { kind: "mock", label: "mock", tip: async () => reopenedScan.scanned,
        blocks: async () => new Uint8Array(), submit: async () => { broadcasts++; return "fixture-txid"; } } });
    t.after(() => reopened.dispose());
    await reopened.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
    nextMode = "reply";
    await reopened.send("uregtest-fixture", "0.00005");
    const fresh = current(); assert.notEqual(fresh, old);
    old.reply(late); old.crash(); await turn();
    assert.equal(fresh.terminated, 0); assert.equal(broadcasts, priorBroadcasts + 1);
    assert.equal(timers.size, 0);
  });

});

test("a multicore scan worker proves in place with its own wallet and proving key", async (t) => {
  class MulticoreScanWorker extends ScanWorker {
    proveReply: { hex?: string; txid?: string; error?: string } = { hex: "fixture-in-place-transaction", txid: "ab".repeat(32) };
    proofRequests: Array<{ kind?: string; maxFeeZat?: string }> = [];
    override postMessage(msg: { id: number; op: string; blob?: ArrayBuffer; kind?: string; maxFeeZat?: string }) {
      if (msg.op === "supportsTransparentSend" || msg.op === "estimateTransparentFee") {
        this.ops.push(msg.op);
        queueMicrotask(() => this.reply(msg.id, msg.op === "supportsTransparentSend"
          ? { supported: true } : { json: JSON.stringify({ feeZat: 10_000 }) }));
        return;
      }
      if (msg.op === "init") {
        this.ops.push(msg.op);
        queueMicrotask(() => this.reply(msg.id, { threads: 16, mode: "multi-thread" }));
        return;
      }
      if (msg.op === "prove" || msg.op === "warmProvingKey") {
        this.ops.push(msg.op);
        if (msg.op === "prove") this.proofRequests.push({ kind: msg.kind, maxFeeZat: msg.maxFeeZat });
        const data = msg.op === "prove" ? this.proveReply : { ready: true, ms: 1 };
        queueMicrotask(() => this.reply(msg.id, data));
        return;
      }
      super.postMessage(msg);
    }
  }
  const scan = new MulticoreScanWorker();
  await restartScanWorker(() => scan as unknown as Worker);
  const db = memoryIndexedDb();
  const priorIdb = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const priorRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: db.indexedDB });
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: db.IDBKeyRange });
  t.after(() => {
    if (priorIdb) Object.defineProperty(globalThis, "indexedDB", priorIdb);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (priorRange) Object.defineProperty(globalThis, "IDBKeyRange", priorRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  const broadcasts: string[] = [];
  const submissions: string[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => scan.scanned, blocks: async () => new Uint8Array(),
    submit: async (hex: string) => { submissions.push("network"); broadcasts.push(hex); return "fixture-txid"; },
  } });
  t.after(() => cancelWasmSync());
  await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", WORKER_BIRTHDAY);
  assert.ok(workerScanSession()?.prove, "multicore sessions prove in place");

  scan.ops.length = 0;
  client.on("broadcast", (event) => { assert.equal(event.kind, "send"); submissions.push("event"); });
  await client.send("uregtest-fixture", "0.00005");
  assert.deepEqual(broadcasts, ["fixture-in-place-transaction"]);
  assert.deepEqual(submissions, ["event", "network"], "submission starts after the event");
  assert.ok(scan.ops.includes("prove"));
  assert.ok(!scan.ops.includes("fromSnapshot"), "a successful proof keeps the worker's built wallet");
  assert.ok(scan.ops.indexOf("persistenceSnapshot") > scan.ops.indexOf("prove"), "the reservation is saved before broadcast");

  scan.proveReply = { error: "insufficient funds" };
  scan.ops.length = 0;
  await assert.rejects(client.send("uregtest-fixture", "0.00005"), /insufficient funds/);
  assert.ok(scan.ops.includes("fromSnapshot"), "a failed in-place proof restores the wallet from before it");
  assert.equal(broadcasts.length, 1);
  assert.deepEqual(submissions, ["event", "network"], "proof failure emits no broadcast");

  scan.proveReply = { hex: "fixture-transparent-transaction", txid: "ab".repeat(32) };
  assert.equal(await client.supportsTransparentSend(), true);
  assert.equal((await client.estimateTransparentFee("transparent-fixture", "0.00005")).feeZat, 10_000);
  await client.sendTransparent("transparent-fixture", "0.00005", { maxFeeZat: "10000" });
  assert.deepEqual(scan.proofRequests.at(-1), { kind: "sendTransparent", maxFeeZat: "10000" });
  assert.equal(broadcasts.at(-1), "fixture-transparent-transaction");

  assert.equal(orchardProvingKeyReady(), false);
  const warmed = await Promise.all([prewarmOrchardProvingKey(), prewarmOrchardProvingKey()]);
  assert.ok(warmed.every(result => result.ready));
  assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 1);
  assert.ok(scan.ops.includes("warmProvingKey"), "the key is built in the instance that proves");
  assert.equal(orchardProvingKeyReady(), true);
  await restartScanWorker(() => new MulticoreScanWorker() as unknown as Worker);
  assert.equal(orchardProvingKeyReady(), false, "a replacement worker has no key");
});

test("wallet background key preparation honors defaults, opt-out and unchanged-tip setting changes", async (t) => {
  class FundedScanWorker extends ScanWorker {
    warmReady = true;
    viewOnly = false;
    balance: Partial<WalletSnapshot["balance"]> = { orchardAvailable: 100_000 };
    override postMessage(msg: { id: number; op: string; blob?: ArrayBuffer }) {
      if (msg.op === "init" || msg.op === "snapshotJson" || msg.op === "warmProvingKey") {
        this.ops.push(msg.op);
        const data = msg.op === "init" ? { threads: 8, mode: "multi-thread" }
          : msg.op === "warmProvingKey" ? { ready: this.warmReady, ms: 1 }
          : { json: JSON.stringify({ network: "regtest", birthdayHeight: 1, scannedHeight: this.scanned,
            viewOnly: this.viewOnly, balance: {
              totalAvailable: Object.values(this.balance).reduce((total, amount) => total + (amount ?? 0), 0),
              orchardAvailable: 0, ironwoodAvailable: 0, saplingAvailable: 0, transparentAvailable: 0,
              ...this.balance,
            } }) };
        queueMicrotask(() => this.reply(msg.id, data));
        return;
      }
      super.postMessage(msg);
    }
  }
  for (const [key, value] of Object.entries({ window: new EventTarget(), document: new EventTarget(),
    Worker: class { constructor() { throw new Error("must use the current scan worker"); } } })) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, value });
    t.after(() => previous ? Object.defineProperty(globalThis, key, previous) : Reflect.deleteProperty(globalThis, key));
  }
  const { createWallet } = await import("../src/create-wallet.ts");
  for (const pool of ["orchardAvailable", "ironwoodAvailable", "transparentAvailable"] as const) {
    for (const gap of [0, 1]) for (const enabled of [undefined, true, false]) {
      await t.test(`${pool}, ${gap ? "new block" : "caught up"}, prewarm ${String(enabled ?? "default")}`, async (st) => {
        const scan = new FundedScanWorker();
        scan.balance = { [pool]: 100_000 };
        await restartScanWorker(() => scan as unknown as Worker);
        let tip = scan.scanned;
        const wallet = await createWallet({ storage: memoryWalletStorage(), unlockPolicy: "session", network: "regtest", autoShield: false,
          ...(enabled === undefined ? {} : { prewarmProvingKey: enabled }),
          memoFetch: "on-demand", server: { kind: "mock", label: "fixture", tip: async () => tip,
            blocks: async () => new Uint8Array([1]) } });
        st.after(() => wallet.close());
        await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: WORKER_BIRTHDAY, replace: true });
        await wallet.setUnlockPolicy("each-spend");
        assert.equal(wallet.hasSpendingSeed(), false, "a locked software wallet can prepare a public proving key");
        tip += gap;
        await wallet.sync();
        await turn();
        const expected = enabled === false ? 0 : 1;
        assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, expected);
        await wallet.sync();
        await turn();
        assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, expected, "a ready key is never rebuilt");
      });
    }
  }
  for (const gap of [0, 1]) for (const kind of ["empty", "sapling-only", "view-only"] as const) {
    await t.test(`${kind}, ${gap ? "new block" : "caught up"} skips background key preparation`, async (st) => {
      const scan = new FundedScanWorker();
      scan.balance = kind === "sapling-only" ? { saplingAvailable: 100_000 }
        : kind === "view-only" ? { transparentAvailable: 100_000 } : {};
      scan.viewOnly = kind === "view-only";
      await restartScanWorker(() => scan as unknown as Worker);
      let tip = scan.scanned;
      const wallet = await createWallet({ storage: memoryWalletStorage(), unlockPolicy: "session", network: "regtest", autoShield: false, prewarmProvingKey: true,
        memoFetch: "on-demand", server: { kind: "mock", label: "fixture", tip: async () => tip,
          blocks: async () => new Uint8Array([1]) } });
      st.after(() => wallet.close());
      await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: WORKER_BIRTHDAY, replace: true });
      await wallet.setUnlockPolicy("each-spend");
      assert.equal(wallet.hasSpendingSeed(), false);
      tip += gap;
      await wallet.sync(); await turn();
      assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 0);
      assert.ok(!scan.ops.includes("prove"), "sync does not start a spend");
    });
  }
  await t.test("live toggles govern retries without a new block", async (st) => {
    const scan = new FundedScanWorker();
    scan.warmReady = false;
    await restartScanWorker(() => scan as unknown as Worker);
    const wallet = await createWallet({ storage: memoryWalletStorage(), unlockPolicy: "session", network: "regtest", autoShield: false, prewarmProvingKey: false,
      memoFetch: "on-demand", server: { kind: "mock", label: "fixture", tip: async () => scan.scanned,
        blocks: async () => { throw new Error("already at tip"); } } });
    st.after(() => wallet.close());
    await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: WORKER_BIRTHDAY, replace: true });
    await wallet.sync(); await turn();
    assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 0);
    wallet.setPrewarmProvingKey(true);
    await wallet.sync(); await turn();
    assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 1);
    assert.equal(orchardProvingKeyReady(), false, "a failed preparation remains eligible for retry");
    wallet.setPrewarmProvingKey(false);
    await wallet.sync(); await turn();
    assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 1, "opt-out suppresses an otherwise eligible retry");
    scan.warmReady = true;
    wallet.setPrewarmProvingKey(true);
    await wallet.sync(); await turn();
    assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 2);
    assert.equal(orchardProvingKeyReady(), true);
    await wallet.sync(); await turn();
    assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 2);
  });
});

test("synchronous synced listeners can cancel before automatic shielding or key preparation", async (t) => {
  class DepositScanWorker extends ScanWorker {
    complete = true;
    override postMessage(msg: { id: number; op: string; blob?: ArrayBuffer }) {
      if (["init", "snapshotJson", "prove", "warmProvingKey"].includes(msg.op)) {
        this.ops.push(msg.op);
        const data = msg.op === "init" ? { threads: 8, mode: "multi-thread" }
          : msg.op === "prove" ? { hex: "fixture-shield-transaction", txid: "ab".repeat(32) }
          : msg.op === "warmProvingKey" ? { ready: true, ms: 1 }
          : { json: JSON.stringify({ network: "regtest", birthdayHeight: 1, scannedHeight: this.scanned,
            transparentScanHeight: this.scanned, transparentScanComplete: this.complete, viewOnly: false,
            balance: { totalAvailable: 100_100_000, orchardAvailable: 100_000, transparentAvailable: 100_000_000 } }) };
        queueMicrotask(() => this.reply(msg.id, data));
        return;
      }
      super.postMessage(msg);
    }
  }
  for (const [key, value] of Object.entries({ window: new EventTarget(), document: new EventTarget(),
    Worker: class { constructor() { throw new Error("must use the current scan worker"); } } })) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, value });
    t.after(() => previous ? Object.defineProperty(globalThis, key, previous) : Reflect.deleteProperty(globalThis, key));
  }
  const { createWallet } = await import("../src/create-wallet.ts");
  for (const policy of ["session", "each-spend"] as const)
  for (const gap of [0, 1]) for (const mode of ["none", "validation-failure", "progress-cancel", "progress-close", "event-cancel", "event-close"]) {
    await t.test(`${policy}, ${gap ? "new block" : "caught up"}, ${mode}`, async (st) => {
      const scan = new DepositScanWorker();
      scan.complete = mode !== "validation-failure";
      await restartScanWorker(() => scan as unknown as Worker);
      let tip = scan.scanned;
      let broadcasts = 0;
      let callbacks = 0;
      let wallet: Awaited<ReturnType<typeof createWallet>>;
      const cancel = () => {
        callbacks++;
        if (mode.endsWith("close")) void wallet.close();
        else wallet.cancelSync();
      };
      wallet = await createWallet({ storage: memoryWalletStorage(), network: "regtest", autoShield: true, prewarmProvingKey: true,
        unlockPolicy: policy, transparentScan: "compact", memoFetch: "on-demand",
        onProgress: event => { if (event.stage === "synced" && mode.startsWith("progress")) cancel(); },
        server: { kind: "mock", label: "fixture", tip: async () => tip,
          blocks: async () => new Uint8Array([1]),
          info: async () => ({ transparentCompact: true, protocolVersion: "v0.5.0" }),
          transparentBlocks: async () => new Uint8Array([1]),
          submit: async () => { broadcasts++; return "fixture-txid"; },
        } });
      st.after(() => wallet.close());
      await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: WORKER_BIRTHDAY, replace: true });
      if (policy === "each-spend") {
        assert.equal(wallet.hasSpendingSeed(), false);
        await wallet.unlock(REGTEST_FAUCET_MNEMONIC);
      }
      wallet.on("sync", event => { if (event.stage === "synced" && mode.startsWith("event")) cancel(); });
      tip += gap;
      if (mode === "validation-failure") {
        await wallet.sync();
        assert.equal(scan.ops.filter(op => op === "prove").length, 0);
        assert.equal(broadcasts, 0);
        assert.equal(wallet.hasSpendingSeed(), policy === "session", "failed shielding did not consume the per-spend unlock");
      } else if (mode === "none") {
        assert.equal((await wallet.sync()).scannedHeight, tip);
        assert.equal(scan.ops.filter(op => op === "prove").length, 1);
        assert.equal(broadcasts, 1, "the real SDK shield path reaches the fixture broadcaster");
        assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 1);
        assert.equal(wallet.hasSpendingSeed(), policy === "session");
        if (policy === "each-spend") {
          await wallet.sync();
          assert.equal(broadcasts, 1, "automatic shielding reused a consumed unlock");
        }
      } else {
        const error = await wallet.sync().then(() => undefined, (reason: unknown) => reason);
        await turn();
        assert.equal(callbacks, 1);
        assert.equal(scan.ops.filter(op => op === "prove").length, 0);
        assert.equal(broadcasts, 0);
        assert.equal(scan.ops.filter(op => op === "warmProvingKey").length, 0);
        assert.match(String(error), /cancelled/);
      }
    });
  }
});

test("finishing public deposit coverage at an unchanged tip auto-shields only when allowed", async (t) => {
  for (const mode of ["session", "each-spend", "disabled"] as const) {
    const worker = new ScanWorker();
    worker.scanned = 10;
    worker.transparentScanHeight = 9;
    const post = worker.postMessage.bind(worker);
    worker.postMessage = (msg) => {
      if (msg.op === "applyTransparentBlocks" && msg.blob?.byteLength) {
        worker.transparentScanHeight = 10;
        worker.transparentAvailable = 100_000_000;
      }
      post(msg);
    };
    await restartScanWorker(() => worker as unknown as Worker);
    const ranges: number[][] = [];
    const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: mode !== "disabled",
      unlockPolicy: mode === "each-spend" ? "each-spend" : "session",
      transparentScan: "compact", memoFetch: "on-demand", transport: {
        kind: "mock", label: "mock", tip: async () => 10,
        blocks: async () => { throw new Error("already at shielded tip"); },
        info: async () => ({ transparentCompact: true, protocolVersion: "v0.5.0" }),
        transparentBlocks: async (start, end) => { ranges.push([start, end]); return new Uint8Array([1]); },
      } });
    await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1);
    let shields = 0;
    t.mock.method(client, "shield", async () => { shields++; return client.getWallet(); });
    const after = await client.sync();
    assert.deepEqual(ranges, [[10, 10]]);
    assert.equal(after.transparentScanStatus, "complete");
    assert.equal(after.balance.transparentAvailable, 100_000_000);
    assert.equal(shields, mode === "session" ? 1 : 0, mode);
    cancelWasmSync();
  }
});

test("createWallet URL servers require a separate shared memo capability", async (t) => {
  class SharedMemoScanWorker extends ScanWorker {
    memoHeight: number | undefined;
    override postMessage(msg: { id: number; op: string; blob?: ArrayBuffer; json?: string }) {
      if (msg.op === "snapshotJson" || msg.op === "applySharedMemos") {
        this.ops.push(msg.op);
        if (msg.op === "applySharedMemos") this.memoHeight = JSON.parse(msg.json!).end;
        const data = msg.op === "applySharedMemos" ? { n: 0 } : { json: JSON.stringify({
          network: "regtest", birthdayHeight: 1, scannedHeight: this.scanned, memoScanHeight: this.memoHeight,
          balance: { totalAvailable: 0, orchardAvailable: 0, transparentAvailable: 0 },
        }) };
        queueMicrotask(() => this.reply(msg.id, data));
        return;
      }
      super.postMessage(msg);
    }
  }
  const { createWallet } = await import("../src/create-wallet.ts");
  for (const enabled of [undefined, false, true]) {
    await t.test(`sharedMemos ${String(enabled ?? "default")}`, async (st) => {
      const worker = new SharedMemoScanWorker(); worker.scanned = 9;
      await restartScanWorker(() => worker as unknown as Worker);
      await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
      const calls: string[] = [];
      st.mock.method(globalThis, "fetch", async (input) => {
        const url = new URL(String(input)); calls.push(url.pathname);
        if (url.pathname === "/zstack/memos") {
          assert.equal(url.search, "?start=1&end=9");
          return Response.json({ start: 1, end: 9, blocks: "", transactions: [] });
        }
        assert.ok(/\/(GetLatestBlock|GetSubtreeRoots)$/.test(url.pathname), "shared mode must not send wallet transaction IDs or addresses");
        return new Response(url.pathname.endsWith("GetLatestBlock") ? new Uint8Array([0, 0, 0, 0, 2, 8, 9]) : new Uint8Array(),
          { headers: { "grpc-status": "0" } });
      });
      const wallet = await createWallet({ storage: memoryWalletStorage(), unlockPolicy: "session", autoShield: true, network: "regtest", server: "https://gateway.example",
        memoFetch: "shared", autoShield: false, prewarmProvingKey: false,
        ...(enabled === undefined ? {} : { sharedMemos: enabled }),
      });
      st.after(() => wallet.close());
      await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1, replace: true });
      for (let pass = 0; pass < 2; pass++) {
        const snapshot = await wallet.sync();
        assert.equal(snapshot.memoFetchStatus, enabled ? "complete" : "unsupported");
        assert.equal(snapshot.sharedMemoStatus, enabled ? "complete" : "unsupported");
      }
      if (!enabled) await assert.rejects(wallet.fetchMemos(), /does not support shared payment notes/);
      assert.equal(calls.filter(path => path === "/zstack/memos").length, enabled ? 1 : 0);
      assert.equal(worker.ops.filter(op => op === "applySharedMemos").length, enabled ? 1 : 0);
      assert.equal(calls.some(path => /GetTransaction|GetAddress|GetTaddress/.test(path)), false);
    });
  }
});

test("cached owners start a scanner before announcing readiness and clean up failed setup", async t => {
  const { createWallet } = await import("../src/create-wallet.ts");
  const { wasmRuntime } = await import("../src/runtime.ts");
  const workers: WarmWorker[] = [];
  class WarmWorker extends EventTarget {
    terminated = false;
    constructor() { super(); workers.push(this); }
    postMessage(message: { id: number; op: string }) {
      queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: {
        id: message.id, ...(message.op === "init" ? { mode: "single-thread", threads: 1 } : {}),
      } })));
    }
    terminate() { this.terminated = true; }
  }
  for (const [key, value] of Object.entries({ window: new EventTarget(), document: new EventTarget(), Worker: WarmWorker })) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, value });
    t.after(() => previous ? Object.defineProperty(globalThis, key, previous) : Reflect.deleteProperty(globalThis, key));
  }
  const options = { network: "regtest" as const, storage: memoryWalletStorage(), autoSync: false,
    prewarmProvingKey: false, server: { kind: "fixture", label: "offline", tip: async () => 1, blocks: async () => new Uint8Array() } };
  for (let reopen = 0; reopen < 2; reopen++) {
    const phases: string[] = [];
    const wallet = await createWallet({ ...options, onLoadProgress: value => {
      if (value.component === "scanner") phases.push(value.phase);
    } });
    try {
      await turn();
      assert.equal(phases[0], "initialize", "cached reopen must not claim ready before the new scanner starts");
      assert.equal(phases.at(-1), "ready");
      assert.equal(wasmRuntime()?.scanner, "ready");
    } finally { await wallet.close(); }
  }
  await assert.rejects(createWallet({ ...options, get autoSync(): false { throw new Error("fixture setup failed after client creation"); } }), /fixture setup failed/);
  assert.equal(workers.at(-1)?.terminated, true, "a failed setup releases its client and scanner before the owner lease");
  assert.equal(wasmRuntime()?.scanner, "main-thread");
});

test("scanner init failure reports fallback once, without a stale ready event", async t => {
  const { observeEngineProgress } = await import("../src/engine-progress.ts");
  const { forgetScanWorkerWallet } = await import("../src/scan-host.ts");
  const phases: string[] = [];
  const off = observeEngineProgress(value => { if (value.component === "scanner") phases.push(value.phase); });
  t.after(async () => { off(); await forgetScanWorkerWallet(); });
  t.mock.method(console, "warn", () => {});
  class FailingWorker extends EventTarget {
    postMessage(message: { id: number }) {
      queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: { id: message.id, error: "fixture init failure" } })));
    }
    terminate() {}
  }
  assert.equal(await attachScanWorker(new FailingWorker() as unknown as Worker, { threads: 1, preferMulticore: false }), null);
  assert.deepEqual(phases, ["initialize", "fallback", "ready"]);
  assert.equal(workerScanSession(), null);
});

test("a startup observer can replace a worker without announcing the retired scanner ready", async t => {
  const { observeEngineProgress } = await import("../src/engine-progress.ts");
  const { forgetScanWorkerWallet, scanWorkerStarting } = await import("../src/scan-host.ts");
  class ReplacementWorker extends EventTarget {
    posted = 0;
    postMessage(message: { id: number }) {
      this.posted++;
      queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: { id: message.id, mode: "single-thread", threads: 1 } })));
    }
    terminate() {}
  }
  const first = new ReplacementWorker(), successor = new ReplacementWorker();
  let replaced: Promise<unknown> | undefined;
  const phases: string[] = [];
  const off = observeEngineProgress(value => {
    if (value.component !== "scanner") return;
    phases.push(value.phase);
    if (value.phase === "initialize" && !replaced) {
      replaced = restartScanWorker(() => successor as unknown as Worker);
    }
  });
  t.after(async () => { off(); await forgetScanWorkerWallet(); });
  assert.equal(await attachScanWorker(first as unknown as Worker, { threads: 1, preferMulticore: false }), null);
  await replaced;
  assert.equal(await scanWorkerStarting(), true);
  assert.equal(first.posted, 0, "retired startup must not send init into a successor generation");
  assert.equal(successor.posted, 1);
  assert.deepEqual(phases, ["initialize", "initialize", "ready"]);
});
