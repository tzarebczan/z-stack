import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { setImmediate as turn } from "node:timers/promises";
import { serialize } from "node:v8";
import { WalletError, type WalletSnapshot } from "@z-stack/core";
import { abortSnapshotWrites, advanceWalletGeneration, clearSavedSnapshot, clearSavedWallet, peekSavedSnapshot, readSavedSnapshot, readSavedSnapshotRecord, readWalletGeneration, saveWalletSnapshot } from "../src/snapshot-storage.ts";
import { prepareSnapshotReplacement } from "../src/snapshot-storage.ts";
import { WALLET_GENERATION_KEY } from "../src/wallet-storage.ts";
import { attachScanWorker, localScanSession, restartScanWorker, workerScanSession } from "../src/scan-host.ts";
import { trackScanRevision } from "../src/scan-revision.ts";
import { attachWasmBindings, cancelWasmSync, createWasmClient, forgetWasmWallet, peekWasmWallet, type WasmProgress } from "../src/wasm-client.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";

const preview = (height = 10) => ({
  network: "regtest", server: "", birthdayHeight: 1, scannedHeight: height,
  unifiedAddress: "uregtest-fixture", transparentAddress: null, zip321: "zcash:uregtest-fixture",
  balance: { totalAvailable: height, orchardAvailable: height },
} as WalletSnapshot);

function installDb(t: TestContext) {
  const db = memoryIndexedDb();
  const previous = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const previousRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { value: db.indexedDB, configurable: true });
  Object.defineProperty(globalThis, "IDBKeyRange", { value: db.IDBKeyRange, configurable: true });
  t.after(() => {
    cancelWasmSync();
    if (previous) Object.defineProperty(globalThis, "indexedDB", previous);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (previousRange) Object.defineProperty(globalThis, "IDBKeyRange", previousRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  return db;
}

test("saved preview reads no snapshot bytes and hydration clones exactly one full snapshot", async (t) => {
  const db = installDb(t);
  const bytes = new Uint8Array(8 * 1024 * 1024).fill(7);
  const wallet = { ...preview(), transactions: [{ txid: "history" }], mnemonic: "must never persist in preview" } as WalletSnapshot;
  await saveWalletSnapshot(bytes, wallet, () => true);
  assert.equal(db.records.size, 259);
  for (const value of db.records.values()) if (Array.isArray(value)) {
    assert.ok(value.length <= 128);
    assert.ok(serialize(value).length < 32 * 1024, "index pages also stay below the large-value threshold");
  }
  db.reads.length = 0;
  const shown = await peekSavedSnapshot(() => { throw new Error("new saves must not parse the snapshot"); });
  assert.equal(shown?.scannedHeight, 10);
  assert.equal(shown?.transactions, undefined);
  assert.equal(shown?.mnemonic, undefined);
  assert.equal(shown?.spendReady, false);
  assert.equal(db.clonedBytes, 0);
  assert.deepEqual(db.reads.filter(key => key !== WALLET_GENERATION_KEY), ["default"]);
  assert.equal(db.keyReads.length, 1);
  assert.deepEqual(await readSavedSnapshot(), bytes);
  assert.equal(db.clonedBytes, bytes.byteLength);
});

test("chunked snapshots preserve exact views, reject missing parts, and replace atomically", async (t) => {
  const db = installDb(t);
  const size = 128 * 1024 + 7;
  const bytes = new Uint8Array(size + 10).subarray(3, 3 + size).fill(7);
  const key = await saveWalletSnapshot(bytes, preview(), () => true);
  const ref = db.records.get("default") as { key: string; byteLength: number; format: string };
  assert.equal(ref.format, "z-stack-snapshot-ref-3");
  assert.equal(ref.byteLength, size);
  for (const [name, payload] of db.records) if (String(name).includes(":c:")) {
    assert.ok(payload instanceof Uint8Array && payload.length <= 32 * 1024);
    assert.equal(payload.buffer.byteLength, payload.byteLength);
  }
  bytes.fill(42);
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array(size).fill(7));
  const chunkKey = `${key}:c:2`;
  const chunk = db.records.get(chunkKey);
  db.records.delete(chunkKey);
  await assert.rejects(readSavedSnapshot(), /snapshot is incomplete/);
  assert.equal(db.records.get("default"), ref, "a failed read must not erase recovery data");
  db.records.set(chunkKey, new Uint8Array(1));
  await assert.rejects(readSavedSnapshot(), /snapshot is incomplete/);
  db.records.set(chunkKey, chunk);
  db.failPutKey = "default";
  await assert.rejects(saveWalletSnapshot(bytes, preview(20), () => true, undefined, key), /quota exceeded/);
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array(size).fill(7));
  assert.equal(db.records.size, 7, "aborted replacement leaves no orphan chunks");
  await saveWalletSnapshot(new Uint8Array([1]), preview(30), () => true, undefined, key);
  assert.equal(db.records.size, 2, "smaller replacement removes every prior chunk");
});

function installLegacyBlob(db: ReturnType<typeof memoryIndexedDb>, bytes: Uint8Array) {
  db.records.clear();
  db.records.set("default", { format: "z-stack-snapshot-ref-1", key: "snapshot:legacy",
    byteLength: bytes.length, preview: preview() });
  db.records.set("snapshot:legacy", new Blob([new Uint8Array(bytes)]));
}

test("legacy ArrayBuffer and bounded typed-array snapshots stay readable", async (t) => {
  const db = installDb(t);
  const bytes = new TextEncoder().encode(JSON.stringify({ magic: "zstk1", network: "regtest",
    unifiedAddress: "uregtest-fixture", birthday: 1, scannedHeight: 9, notes: [] }));
  db.records.set("default", bytes.buffer);
  assert.equal((await peekWasmWallet())?.scannedHeight, 9);
  assert.deepEqual(await readSavedSnapshot(), bytes);
  const backing = new Uint8Array(bytes.byteLength + 8).fill(99);
  backing.set(bytes, 3);
  db.records.set("default", backing.subarray(3, 3 + bytes.byteLength));
  const restored = await readSavedSnapshot();
  assert.deepEqual(restored, bytes);
  assert.equal(restored?.byteOffset, 0);
  assert.equal(restored?.buffer.byteLength, bytes.byteLength);
  await saveWalletSnapshot(bytes, preview(), () => true);
  const ref = db.records.get("default") as { key: string };
  db.records.set(ref.key, bytes.buffer);
  assert.deepEqual(await readSavedSnapshot(), bytes, "the previous manifest + ArrayBuffer payload also loads");
});

test("Blob materialization failure preserves the committed pair and a later read retries", async (t) => {
  const db = installDb(t);
  const bytes = new Uint8Array(16 * 1024 * 1024).fill(7);
  installLegacyBlob(db, bytes);
  const arrayBuffer = Blob.prototype.arrayBuffer;
  let fail = true;
  t.mock.method(Blob.prototype, "arrayBuffer", function(this: Blob) {
    if (fail) { fail = false; return Promise.reject(new Error("blob read failed")); }
    return arrayBuffer.call(this);
  });
  await assert.rejects(readSavedSnapshot(), /blob read failed/);
  assert.equal(db.records.size, 2);
  assert.equal((await peekWasmWallet())?.scannedHeight, 10);
  assert.deepEqual(await readSavedSnapshot(), bytes);
});

test("a legacy Blob at the default key can be replaced when its length matches", async (t) => {
  const db = installDb(t);
  const legacy = new Uint8Array(64).fill(9);
  db.records.set("default", new Blob([legacy]));
  await assert.rejects(
    saveWalletSnapshot(new Uint8Array([3]), preview(13), () => true, undefined, null, true, new Uint8Array(legacy.byteLength + 1)),
    (error: unknown) => error instanceof WalletError && error.code === "wallet_changed",
  );
  assert.ok(db.records.get("default") instanceof Blob);
  const key = await saveWalletSnapshot(new Uint8Array([1, 2]), preview(12), () => true, undefined, null, true, legacy);
  assert.equal(typeof key, "string");
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array([1, 2]));
});

test("legacy snapshot preview and hydration survive migration, including an old-client overwrite", async (t) => {
  const db = installDb(t);
  const legacy = new TextEncoder().encode(JSON.stringify({ magic: "zstk1", network: "regtest",
    unifiedAddress: "uregtest-fixture", birthday: 1, scannedHeight: 9, notes: [] }));
  db.records.set("default", legacy);
  assert.equal((await peekWasmWallet())?.scannedHeight, 9);
  assert.deepEqual(await readSavedSnapshot(), legacy);
  await saveWalletSnapshot(new Uint8Array([2, 3]), preview(10), () => true);
  assert.equal((await peekWasmWallet())?.scannedHeight, 10);
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array([2, 3]));
  // Older builds write raw bytes to default; their update must never show our old preview.
  db.records.set("default", legacy);
  assert.equal((await peekWasmWallet())?.scannedHeight, 9);
  await saveWalletSnapshot(new Uint8Array([4, 5]), preview(11), () => true);
  assert.equal(db.records.size, 2, "the next save removes any orphan blob left by an old client");
});

test("missing or malformed snapshot references cannot display an orphan preview", async (t) => {
  const db = installDb(t);
  await saveWalletSnapshot(new Uint8Array([1]), preview(), () => true);
  const ref = db.records.get("default") as { key: string };
  db.records.delete(ref.key);
  assert.equal(await peekWasmWallet(), null);
  assert.equal(await readSavedSnapshot(), null);
  db.records.set("default", { format: "z-stack-snapshot-ref-1", key: "passkey.v1", preview: preview(), byteLength: 1 });
  assert.equal(await peekWasmWallet(), null);
  assert.equal(await readSavedSnapshot(), null);
  db.records.set("default", new TextEncoder().encode("bad legacy snapshot"));
  assert.equal(await peekSavedSnapshot(() => { throw new Error("malformed snapshot"); }), null);
  assert.equal(await peekWasmWallet(), null);
});

test("failed transaction preserves the previous snapshot and preview atomically", async (t) => {
  const db = installDb(t);
  await saveWalletSnapshot(new Uint8Array([1]), preview(10), () => true);
  db.failPutKey = "default";
  await assert.rejects(saveWalletSnapshot(new Uint8Array([2]), preview(20), () => true), /quota exceeded/);
  assert.equal((await peekWasmWallet())?.scannedHeight, 10);
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array([1]));
  assert.equal(db.records.size, 2, "aborted replacement must not leak its blob");
  await saveWalletSnapshot(new Uint8Array([3]), preview(30), () => true);
  assert.equal((await peekWasmWallet())?.scannedHeight, 30);
  assert.equal(db.records.size, 2, "successful replacement drops the prior blob");
  await assert.rejects(saveWalletSnapshot(new Uint8Array([4]), { ...preview(40), network: "bad" }, () => true), /invalid snapshot preview/);
  assert.equal((await peekWasmWallet())?.scannedHeight, 30, "invalid metadata cannot replace a good saved wallet");
});

test("cancellation aborts an in-flight IDB transaction and invalidates an outstanding preview", async (t) => {
  const db = installDb(t);
  await saveWalletSnapshot(new Uint8Array([1]), preview(10), () => true);
  let current = true;
  db.holdCommits = true;
  const save = saveWalletSnapshot(new Uint8Array([2]), preview(20), () => current);
  await turn();
  assert.equal(db.commits.length, 1);
  current = false;
  abortSnapshotWrites();
  await save;
  db.commits.shift()!();
  db.holdCommits = false;
  assert.equal((await peekWasmWallet())?.scannedHeight, 10);
  db.holdOpens = true;
  const pending = peekWasmWallet();
  await turn();
  cancelWasmSync();
  db.opens.shift()!();
  assert.equal(await pending, null);
});

test("concurrent saves keep one matching pair and forget removes both while preserving other records", async (t) => {
  const db = installDb(t);
  db.records.set("passkey.v1", { credential: "retained" });
  db.records.set("meta", { unlockPolicy: "session" });
  db.records.set("snapshot:orphan", new Uint8Array([99]));
  await Promise.all(Array.from({ length: 5 }, (_, i) => saveWalletSnapshot(new Uint8Array([i]), preview(i), () => true)));
  assert.equal((await peekWasmWallet())?.scannedHeight, 4);
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array([4]));
  assert.equal(db.records.size, 4);
  await clearSavedSnapshot();
  assert.equal(await peekWasmWallet(), null);
  assert.equal(await readSavedSnapshot(), null);
  assert.deepEqual([...db.records.keys()], ["passkey.v1", WALLET_GENERATION_KEY]);
});

test("a save based on an older revision fails instead of overwriting another tab's save", async (t) => {
  installDb(t);
  const k0 = await saveWalletSnapshot(new Uint8Array([0]), preview(0), () => true, undefined, null);
  assert.ok(k0);
  // Tab A saves a pending send on top of k0.
  const k1 = await saveWalletSnapshot(new Uint8Array([1]), preview(1), () => true, undefined, k0);
  assert.ok(k1 && k1 !== k0);
  // Tab B still holds k0: its save must not replace A's.
  await assert.rejects(
    saveWalletSnapshot(new Uint8Array([2]), preview(2), () => true, undefined, k0),
    (e: unknown) => e instanceof WalletError && e.code === "wallet_changed",
  );
  // Nor may a tab that never saw a revision (a legacy load) overwrite one.
  await assert.rejects(
    saveWalletSnapshot(new Uint8Array([2]), preview(2), () => true, undefined, null),
    (e: unknown) => e instanceof WalletError && e.code === "wallet_changed",
  );
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array([1]));
  assert.equal((await readSavedSnapshotRecord())?.key, k1);
});

test("another wallet generation's leftover does not block the new wallet's first save", async (t) => {
  installDb(t);
  await saveWalletSnapshot(new Uint8Array([0]), preview(0), () => true, undefined, null);
  const next = await advanceWalletGeneration(undefined, await readWalletGeneration());
  assert.ok(await saveWalletSnapshot(new Uint8Array([9]), preview(9), () => true, next, null));
  assert.deepEqual(await readSavedSnapshot(), new Uint8Array([9]));
});

test("local persistence captures snapshot and preview before another job can mutate the wallet", async () => {
  let height = 4;
  const session = localScanSession({
    toSnapshot: () => { queueMicrotask(() => { height = 5; }); return new Uint8Array([height]); },
    snapshotJson: () => JSON.stringify(preview(height)),
  } as never);
  const captured = await session.persistenceSnapshot();
  assert.equal(height, 5);
  assert.equal(captured.bytes[0], 4);
  assert.equal(JSON.parse(captured.previewJson).scannedHeight, 4);
});

test("device forget frees local Rust wallets and old clients cannot resurrect the saved wallet", async (t) => {
  const db = installDb(t);
  let freed = 0;
  attachWasmBindings({
    WasmWallet: { create: () => ({ free: () => { freed++; }, toSnapshot: () => new Uint8Array([1]),
      snapshotJson: () => JSON.stringify(preview()), scannedHeight: () => 10 }) },
  } as never);
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
  } });
  await client.restore("test-only local fixture", "regtest", 1);
  await forgetWasmWallet();
  assert.equal(freed, 1);
  await assert.rejects(client.getWallet(), /no wasm wallet/);
  await assert.rejects(client.nextAddress!(), /no wasm wallet/);
  assert.equal((await client.health()).wallet, false);
  assert.deepEqual([...db.records.keys()], [WALLET_GENERATION_KEY]);
});

class MemoWorker extends EventTarget {
  scanned = 10;
  birthday = 1;
  transparentSupported = true;
  legacyEnhancement = false;
  transparentHeight: number | undefined;
  memoHeight: number | undefined;
  combinedApplies: boolean[] = [];
  pending: string[] = [];
  completed = new Set<string>();
  enhanced: string[] = [];
  candidates = ["blank", "retry", "missing"];
  ops: string[] = [];
  postMessage(msg: { id: number; op: string; hex?: string; snapshot?: ArrayBuffer; limit?: number; transparent?: boolean }) {
    this.ops.push(msg.op);
    let data: Record<string, unknown> = {};
    const snapshot = () => new TextEncoder().encode(JSON.stringify([...this.completed]));
    switch (msg.op) {
      case "init": data = { threads: 1, mode: "single-thread" }; break;
      case "meta": data = { scanned: this.scanned, nextHeight: this.scanned + 1, birthday: this.birthday, treesReady: true, sinsemillaLive: true, transparentCompact: this.transparentSupported }; break;
      case "applyBlob": this.combinedApplies.push(!!msg.transparent); if (msg.transparent) this.transparentHeight = this.scanned + 1; data = { scanned: ++this.scanned }; break;
      case "snapshotJson": data = { json: JSON.stringify({ ...preview(this.scanned), transparentScanHeight: this.transparentHeight, memoScanHeight: this.memoHeight }) }; break;
      case "persistenceSnapshot": data = { snapshot: snapshot().buffer, json: JSON.stringify(preview(this.scanned)) }; break;
      case "fromSnapshot": this.completed = new Set(JSON.parse(new TextDecoder().decode(msg.snapshot))); break;
      case "history": data = { json: JSON.stringify(this.legacyEnhancement ? this.candidates.filter(id => !this.completed.has(id)).map(txid => ({ txid, receivedNoteCount: 1 })) : []) }; break;
      case "pendingRawTxs": data = { json: JSON.stringify(this.pending) }; break;
      case "memoEnhancementTxids": data = { json: this.legacyEnhancement ? null : JSON.stringify(this.candidates.filter((txid) => !this.completed.has(txid)).slice(0, msg.limit)) }; break;
      case "enhanceRawTx": this.enhanced.push(msg.hex!); this.completed.add(msg.hex!); data = { n: 0 }; break;
    }
    queueMicrotask(() => this.dispatchEvent(new MessageEvent("message", { data: { id: msg.id, ...data } })));
  }
  terminate() {}
}

test("already-caught-up sync reports synced only after pending work and its snapshot commit", async (t) => {
  const db = installDb(t); const worker = new MemoWorker(); worker.candidates = [];
  worker.pending = ["pending-raw"];
  attachWasmBindings({} as never);
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  let release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  let submitted!: () => void;
  const submitting = new Promise<void>(resolve => { submitted = resolve; });
  const progress: WasmProgress[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => 10,
    blocks: async () => { throw new Error("already caught up"); },
    submit: async () => { submitted(); await pending; return "ok"; },
  } }, event => { progress.push(event); });
  await client.restore("test-only worker fixture", "regtest", 1);
  await workerScanSession()!.enhanceRawTx("dirty-before-sync");
  db.holdCommits = true;
  const syncing = client.sync();
  await submitting;
  assert.equal(progress.some(event => event.stage === "synced"), false);
  release();
  for (let i = 0; i < 200 && !db.commits.length; i++) await turn();
  assert.equal(db.commits.length, 1);
  assert.equal(progress.some(event => event.stage === "synced"), false, "a pending save is not completion");
  db.holdCommits = false; db.commits.shift()!();
  const wallet = await syncing;
  assert.deepEqual(progress.filter(event => event.stage === "synced"), [{
    stage: "synced", scanned: 10, downloaded: 10, tip: 10, notesFound: 0, spendsFound: 0,
    percent: 100, remainingSeconds: 0, remainingHuman: "done", message: "synced",
  }]);
  assert.equal(wallet.scannedHeight, 10, "next scan height 11 must not be reported as a scanned block");
  assert.deepEqual(JSON.parse(new TextDecoder().decode(await readSavedSnapshot())), ["dirty-before-sync"]);
  await client.sync();
  assert.equal(progress.filter(event => event.stage === "synced").length, 2, "an unchanged checkpoint still finishes each sync");
});

test("already-caught-up sync cannot report synced when cancelled during its final snapshot read", async (t) => {
  installDb(t); const worker = new MemoWorker(); worker.candidates = [];
  attachWasmBindings({} as never);
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  const progress: WasmProgress[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => 10,
    blocks: async () => { throw new Error("already caught up"); },
    submit: async () => "ok",
  } }, event => { progress.push(event); });
  await client.restore("test-only worker fixture", "regtest", 1);
  const post = worker.postMessage.bind(worker);
  let afterPending = false;
  let release!: () => void;
  let reading!: () => void;
  const started = new Promise<void>(resolve => { reading = resolve; });
  t.mock.method(worker, "postMessage", (message: Parameters<MemoWorker["postMessage"]>[0]) => {
    if (message.op === "pendingRawTxs") afterPending = true;
    if (afterPending && message.op === "snapshotJson") {
      release = () => post(message);
      reading();
    } else post(message);
  });
  const syncing = client.sync();
  const rejected = assert.rejects(syncing, /cancelled/);
  await started;
  cancelWasmSync();
  release();
  await rejected;
  assert.equal(progress.some(event => event.stage === "synced"), false);
});

test("unchanged sync skips serialization and writes, while new blocks checkpoint once", async (t) => {
  const db = installDb(t); const worker = new MemoWorker(); worker.candidates = [];
  worker.transparentHeight = 10;
  worker.memoHeight = 10;
  attachWasmBindings({} as never);
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  let tip = 10;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transparentScan: "compact", transport: {
    kind: "mock", label: "mock", tip: async () => tip, blocks: async () => new Uint8Array([1]),
    info: async () => ({ chain: "regtest", protocolVersion: "v0.5.0", transparentCompact: true }),
    transparentBlocks: async () => new Uint8Array([1]), tx: async () => { throw new Error("no memo requests expected"); },
    sharedMemos: async () => { throw new Error("no shared ranges expected"); },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  worker.ops.length = 0; db.writes.length = 0;
  await client.sync(); await client.sync();
  client.setMemoFetch("shared");
  assert.equal((await client.sync()).sharedMemoStatus, "complete");
  client.setMemoFetch("auto");
  assert.equal(worker.ops.filter(op => op === "persistenceSnapshot").length, 0);
  assert.equal(db.writes.length, 0);
  assert.ok(db.reads.includes("default"), "skipped writes still validate the saved revision");
  tip = 11;
  await client.sync();
  assert.equal(worker.ops.filter(op => op === "persistenceSnapshot").length, 1);
  assert.equal(db.writes.filter(key => key === "default").length, 1);
});

test("unchanged checkpoints observe another tab's save and Forget", async (t) => {
  const db = installDb(t);
  for (const forget of [false, true]) {
    const worker = new MemoWorker(); worker.candidates = [];
    attachWasmBindings({} as never);
    await restartScanWorker(() => worker as unknown as Worker);
    await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
    const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
      kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    } });
    await client.restore("test-only worker fixture", "regtest", 1);
    if (forget) await clearSavedWallet();
    else await saveWalletSnapshot(new TextEncoder().encode('["other-tab"]'), preview(), () => true, await readWalletGeneration());
    const before = db.writes.length;
    await assert.rejects(client.sync(), (e: unknown) => e instanceof WalletError && e.code === "wallet_changed");
    assert.equal(db.writes.length, before, "a skipped checkpoint must not overwrite the newer state");
    if (!forget) assert.ok(worker.completed.has("other-tab"), "the newer state is adopted");
  }
});

test("a forgotten wallet stops automatic memo requests with or without deposit scanning", async (t) => {
  installDb(t);
  for (const transparentScan of ["compact", "off"] as const) {
    const worker = new MemoWorker(); worker.transparentHeight = 10;
    attachWasmBindings({} as never);
    await restartScanWorker(() => worker as unknown as Worker);
    await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
    let queries = 0;
    const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transparentScan, transport: {
      kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
      info: async () => ({ chain: "regtest", protocolVersion: "v0.5.0", transparentCompact: true }),
      transparentBlocks: async () => new Uint8Array(), tx: async id => { queries++; return id; },
    } });
    await client.restore("test-only worker fixture", "regtest", 1);
    await clearSavedWallet();
    await assert.rejects(client.sync(), (e: unknown) => e instanceof WalletError && e.code === "wallet_changed");
    assert.equal(queries, 0, "wallet identity failures are not retrievable-server outages");
  }
});

test("a mutation during an IndexedDB commit remains dirty for the next checkpoint", async (t) => {
  const db = installDb(t); const worker = new MemoWorker(); worker.candidates = ["first"];
  attachWasmBindings({} as never);
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(), tx: async id => id,
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  db.holdCommits = true;
  const saving = client.fetchMemos();
  for (let i = 0; i < 200 && !db.commits.length; i++) await turn();
  assert.equal(db.commits.length, 1);
  await workerScanSession()!.enhanceRawTx("during-commit");
  db.holdCommits = false; db.commits.shift()!();
  await saving;
  assert.deepEqual(JSON.parse(new TextDecoder().decode(await readSavedSnapshot())), ["first"]);
  await client.fetchMemos();
  assert.deepEqual(JSON.parse(new TextDecoder().decode(await readSavedSnapshot())), ["first", "during-commit"]);
});

test("a mutation during a skipped checkpoint's revision check is saved", async (t) => {
  const db = installDb(t); const worker = new MemoWorker(); worker.candidates = [];
  attachWasmBindings({} as never);
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  db.holdReadCommits = true;
  const syncing = client.sync();
  for (let i = 0; i < 200 && !db.commits.length; i++) await turn();
  assert.equal(db.commits.length, 1);
  await workerScanSession()!.enhanceRawTx("during-check");
  db.holdReadCommits = false; db.commits.shift()!();
  await syncing;
  assert.deepEqual(JSON.parse(new TextDecoder().decode(await readSavedSnapshot())), ["during-check"]);
});

test("revision tracking covers partial failures, pending operations and future mutations", async () => {
  let release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const source = trackScanRevision({
    history: async () => "[]",
    enhanceRawTx: async () => { await pending; throw new Error("partially applied"); },
    futureMutation: async () => {},
  } as never) as ReturnType<typeof trackScanRevision> & { futureMutation: () => Promise<void> };
  const first = source.persistenceRevision!();
  await source.history(10);
  assert.equal(source.persistenceRevision!(), first);
  const mutating = source.enhanceRawTx("raw");
  assert.equal(source.persistenceRevision!(), undefined);
  release(); await assert.rejects(mutating, /partially applied/);
  assert.notEqual(source.persistenceRevision!(), first);
  const second = source.persistenceRevision!();
  await source.futureMutation();
  assert.notEqual(source.persistenceRevision!(), second);
});

test("on-demand memos keep both sync paths and pending rebroadcasts while history reads stay local", async (t) => {
  installDb(t);
  const worker = new MemoWorker();
  worker.pending = ["pending-raw"];
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  const txids: string[] = [];
  const submissions: string[] = [];
  let blocks = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => 11,
    blocks: async () => { blocks++; return new Uint8Array([1]); },
    tx: async (id) => { txids.push(id); return id; },
    submit: async (hex) => { submissions.push(hex); return "ok"; },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  await client.sync();
  await client.sync();
  await client.history();
  await client.transaction("blank");
  await client.pending();
  assert.equal(blocks, 1);
  assert.deepEqual(submissions, ["pending-raw", "pending-raw"]);
  assert.deepEqual(txids, []);
  assert.equal(worker.ops.includes("memoEnhancementTxids"), false);
  const snapshot = await client.fetchMemos();
  assert.equal(snapshot.scannedHeight, 11);
  assert.deepEqual(txids, ["blank", "retry", "missing"]);
  assert.deepEqual(worker.enhanced, txids);
  assert.equal(submissions.length, 2, "manual memo loading does not rebroadcast");
  assert.deepEqual(JSON.parse(new TextDecoder().decode(await readSavedSnapshot())), txids);
  client.setMemoFetch("shared");
  assert.equal((await client.getWallet()).sharedMemoStatus, "scanning");
  await assert.rejects(client.fetchMemos(), /does not support shared payment notes/);
  assert.equal((await client.getWallet()).sharedMemoStatus, "unsupported");
  assert.deepEqual(txids, ["blank", "retry", "missing"], "manual shared fetch never falls back to wallet txids");
  client.setMemoFetch("on-demand");
  assert.equal((await client.getWallet()).sharedMemoStatus, "off");
  client.setMemoFetch("shared");
  assert.equal((await client.getWallet()).sharedMemoStatus, "scanning");
});

test("turning automatic memos off stops further batches, and explicit fetch can finish afterward", async (t) => {
  installDb(t);
  const worker = new MemoWorker();
  worker.candidates = Array.from({ length: 12 }, (_, i) => `tx${i}`);
  await restartScanWorker(() => worker as unknown as Worker);
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => { release = resolve; });
  let firstBatch!: () => void;
  const started = new Promise<void>((resolve) => { firstBatch = resolve; });
  const txids: string[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: async (id) => { txids.push(id); firstBatch(); await blocked; return id; },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  const sync = client.sync();
  await started;
  client.setMemoFetch("on-demand");
  release();
  await sync;
  assert.equal(txids.length, 8, "requests already sent cannot be undone; later batches are suppressed");
  assert.deepEqual(worker.enhanced, []);
  await client.sync();
  assert.equal(txids.length, 8);
  await client.fetchMemos();
  assert.deepEqual(worker.enhanced, worker.candidates);
  worker.candidates.push("later");
  client.setMemoFetch("auto");
  await client.sync();
  assert.equal(worker.enhanced.at(-1), "later");
});

test("manual memo loading deduplicates callers, serializes sync, and cannot persist after forget", async (t) => {
  installDb(t);
  const worker = new MemoWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => { release = resolve; });
  let firstFetch!: () => void;
  const started = new Promise<void>((resolve) => { firstFetch = resolve; });
  let tips = 0;
  let fetched = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => { tips++; return 10; }, blocks: async () => new Uint8Array(),
    tx: async (id) => { fetched++; firstFetch(); await blocked; return id; },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  const reading = client.fetchMemos();
  assert.equal(client.fetchMemos(), reading);
  const rejected = assert.rejects(reading, /cancelled|forgotten/);
  await started;
  const syncing = client.sync();
  const syncRejected = assert.rejects(syncing, /cancelled|forgotten/);
  await turn();
  assert.equal(tips, 1, "sync must wait for memo persistence");
  await forgetWasmWallet();
  release();
  await Promise.all([rejected, syncRejected]);
  assert.equal(fetched, 3);
  assert.deepEqual(worker.enhanced, []);
  assert.equal(await readSavedSnapshot(), null);
});

test("manual memo loading rejects an obsolete durable generation before leaking transaction IDs", async (t) => {
  installDb(t);
  const worker = new MemoWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  let fetched = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: async (id) => { fetched++; return id; },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  // Another tab advances the durable generation without this tab's in-memory lifecycle.
  const next = await advanceWalletGeneration();
  await saveWalletSnapshot(new TextEncoder().encode("[]"), preview(), () => true, next);
  await assert.rejects(client.fetchMemos(), /replaced|forgotten/);
  assert.equal(fetched, 0);
});

test("successful empty memo enhancement persists across reload while failed and missing fetches retry", async (t) => {
  installDb(t);
  const worker = new MemoWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const calls: string[] = [];
  let allowRetry = false;
  const opts = { unlockPolicy: "session" as const, memoFetch: "auto" as const, network: "regtest" as const, autoShield: false, transport: {
    label: "mock", kind: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: async (txid: string) => {
      calls.push(txid);
      if (txid === "retry" && !allowRetry) throw new Error("temporary fetch failure");
      return txid === "missing" ? null : txid;
    },
  } };
  const client = createWasmClient(opts);
  await client.restore("test-only worker fixture", "regtest", 1);
  await client.sync();
  assert.deepEqual(worker.enhanced, ["blank"]);
  const reloaded = new MemoWorker();
  await restartScanWorker(() => reloaded as unknown as Worker);
  const nextClient = createWasmClient(opts);
  allowRetry = true;
  await nextClient.sync();
  assert.deepEqual(reloaded.enhanced, ["retry"]);
  assert.deepEqual(calls, ["blank", "retry", "missing", "retry", "missing"]);
  assert.equal(reloaded.completed.has("blank"), true);
  await nextClient.sync();
  assert.equal(calls.at(-1), "missing", "unavailable raw transactions remain eligible");
  assert.equal(calls.filter((txid) => txid === "blank").length, 1);
});

test("cancellation during memo downloads cannot enhance or persist the replacement wallet", async (t) => {
  const db = installDb(t);
  const worker = new MemoWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  let release!: (hex: string) => void;
  const blocked = new Promise<string>((resolve) => { release = resolve; });
  const progress: WasmProgress[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(), tx: async () => blocked,
  } }, event => { progress.push(event); });
  await client.restore("test-only worker fixture", "regtest", 1);
  const syncing = client.sync();
  const rejection = assert.rejects(syncing, /cancelled/);
  await turn();
  cancelWasmSync();
  release("blank");
  await rejection;
  assert.deepEqual(worker.enhanced, []);
  assert.equal(db.writes.filter((key) => key === "default").length, 1);
  assert.equal(progress.some(event => event.stage === "synced"), false);
});

test("forget during a pending snapshot read cannot rehydrate the forgotten wallet", async (t) => {
  const db = installDb(t);
  const worker = new MemoWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  await saveWalletSnapshot(new TextEncoder().encode("[]"), preview(), () => true);
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
  } });
  await turn(); // Metadata read settles before holding the full wallet read.
  db.holdReadCommits = true;
  const loading = client.tip();
  await turn();
  assert.equal(db.commits.length, 1);
  const forgetting = forgetWasmWallet();
  await turn();
  db.holdReadCommits = false;
  db.commits.shift()!();
  await forgetting;
  assert.equal((await loading).scanned, 0);
  assert.equal(worker.ops.filter((op) => op === "fromSnapshot").length, 0);
  assert.equal(await readSavedSnapshot(), null);
});

test("forget during asynchronous Blob hydration cannot reattach the forgotten wallet", async (t) => {
  const db = installDb(t);
  const worker = new MemoWorker();
  await restartScanWorker(() => worker as unknown as Worker);
  const bytes = new Uint8Array(16 * 1024 * 1024).fill(32);
  bytes.set(new TextEncoder().encode("[]"));
  installLegacyBlob(db, bytes);
  let release!: (value: ArrayBuffer) => void;
  let converting!: () => void;
  const started = new Promise<void>((resolve) => { converting = resolve; });
  t.mock.method(Blob.prototype, "arrayBuffer", () => {
    converting();
    return new Promise<ArrayBuffer>((resolve) => { release = resolve; });
  });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
  } });
  const loading = client.tip();
  await started;
  await forgetWasmWallet();
  release(bytes.buffer);
  assert.equal((await loading).scanned, 0);
  assert.equal(worker.ops.filter((op) => op === "fromSnapshot").length, 0);
  assert.equal(await readSavedSnapshot(), null);
});

test("forget rolls back snapshot, seed, passkey and generation together on any delete failure", async (t) => {
  const db = installDb(t);
  await saveWalletSnapshot(new Uint8Array([1]), preview(10), () => true);
  db.records.set("seed.enc", { ct: "ciphertext" });
  db.records.set("passkey.v1", { ct: "prf ciphertext" });
  db.records.set("meta", { unlockPolicy: "session" });
  const before = structuredClone(db.records);
  const generation = await readWalletGeneration();
  for (const failure of ["request", "synchronous", "abort", "construction"] as const) {
    if (failure === "request") db.failDeleteKey = "seed.enc";
    if (failure === "synchronous") db.failDeleteSyncKey = "passkey.v1";
    if (failure === "abort") db.abortNextCommit = true;
    if (failure === "construction") db.failTransaction = true;
    await assert.rejects(clearSavedWallet({ passkey: true }), /delete|aborted|creation/);
    assert.deepEqual(db.records, before, `${failure} must preserve all committed records`);
    assert.equal(await readWalletGeneration(), generation);
    assert.equal(db.closes, db.openCount, "every failed transaction closes its handle");
  }
  await clearSavedWallet();
  assert.deepEqual(db.records.get("passkey.v1"), before.get("passkey.v1"));
  assert.deepEqual([...db.records.keys()].sort(), ["passkey.v1", WALLET_GENERATION_KEY].sort());
  assert.notEqual(await readWalletGeneration(), generation);
  await clearSavedWallet({ passkey: true });
  assert.deepEqual([...db.records.keys()], [WALLET_GENERATION_KEY]);
});

test("a stale other-tab save cannot resurrect a forgotten wallet or erase a newer wallet", async (t) => {
  const db = installDb(t);
  await saveWalletSnapshot(new Uint8Array([1]), preview(10), () => true);
  const tabA = await readSavedSnapshotRecord();
  assert.ok(tabA);
  db.holdOpens = true;
  // This tab's local lifecycle remains current: only the durable fence can stop it.
  const stale = saveWalletSnapshot(new Uint8Array([2]), preview(20), () => true, tabA.generation);
  const rejected = assert.rejects(stale, /saved wallet changed/);
  await turn();
  assert.equal(db.opens.length, 1);
  db.holdOpens = false;
  await clearSavedWallet();
  const next = await advanceWalletGeneration();
  await saveWalletSnapshot(new Uint8Array([3]), preview(30), () => true, next);
  const committed = structuredClone(db.records);
  db.opens.shift()!();
  await rejected;
  assert.deepEqual(db.records, committed, "stale rejection must never delete the newer committed pair");
  assert.equal((await peekWasmWallet())?.scannedHeight, 30);
  const saved = await readSavedSnapshotRecord();
  assert.deepEqual({ bytes: saved?.bytes, generation: saved?.generation }, { bytes: new Uint8Array([3]), generation: next });
  assert.match(saved?.key ?? "", /^snapshot:/, "each save's key is its revision");
});

test("an interrupted generation claim repairs the old wallet without losing pending bytes", async (t) => {
  const db = installDb(t);
  const bytes = encodePending();
  await saveWalletSnapshot(bytes, preview(10), () => true);
  const prior = await readSavedSnapshotRecord();
  assert.ok(prior);
  const before = structuredClone(db.records);
  await advanceWalletGeneration(); // A crashed page from the earlier claim-before-save flow.
  assert.equal((await peekWasmWallet())?.scannedHeight, 10);
  assert.deepEqual(await readSavedSnapshotRecord(), prior);
  assert.deepEqual(db.records, before, "repair changes no snapshot, vault or pending data");
  await clearSavedWallet();
  assert.equal(await readSavedSnapshotRecord(), null, "forget is not repaired into a wallet");
});

function encodePending() { return new TextEncoder().encode(JSON.stringify({ pending: [{ txid: "uncertain-broadcast" }] })); }

for (const changed of ["save", "forget"] as const) {
  test(`prepared replacement cannot erase another tab's ${changed}`, async t => {
    const db = installDb(t);
    await saveWalletSnapshot(new Uint8Array([1]), preview(10), () => true);
    const prior = (await readSavedSnapshotRecord())!;
    const claim = await prepareSnapshotReplacement(new AbortController().signal, prior.generation, true);
    assert.equal(await readWalletGeneration(), prior.generation, "preparation is read-only");
    assert.deepEqual(await readSavedSnapshotRecord(), prior, "another reader still sees the old wallet");
    if (changed === "save") await saveWalletSnapshot(encodePending(), preview(20), () => true, prior.generation, prior.key);
    else await clearSavedWallet();
    const current = structuredClone(db.records);
    await assert.rejects(saveWalletSnapshot(new Uint8Array([3]), preview(30), () => true, claim.generation,
      null, false, undefined, undefined, undefined, true, claim), /saved wallet changed|saved by another tab/);
    assert.deepEqual(db.records, current);
  });
}

test("replacement identity, snapshot and policy commit atomically and abort together", async t => {
  const db = installDb(t);
  await saveWalletSnapshot(new Uint8Array([1]), preview(10), () => true);
  const prior = (await readSavedSnapshotRecord())!;
  const claim = await prepareSnapshotReplacement(new AbortController().signal, prior.generation, true);
  const before = structuredClone(db.records);
  db.failPutKey = WALLET_GENERATION_KEY;
  await assert.rejects(saveWalletSnapshot(new Uint8Array([2]), preview(20), () => true, claim.generation,
    null, false, undefined, "each-spend", undefined, true, claim), /quota exceeded/);
  assert.deepEqual(db.records, before);
  await saveWalletSnapshot(new Uint8Array([2]), preview(20), () => true, claim.generation,
    null, false, undefined, "each-spend", undefined, true, claim);
  assert.equal(await readWalletGeneration(), claim.generation);
  assert.deepEqual((await readSavedSnapshotRecord())?.bytes, new Uint8Array([2]));
  assert.equal((await peekWasmWallet())?.unlockPolicy, "each-spend");
  assert.deepEqual(db.records.get("meta"), { unlockPolicy: "each-spend" });
});


test("a delayed other-tab replacement cannot claim a generation captured before forget", async (t) => {
  const db = installDb(t);
  const baseline = await readWalletGeneration();
  await clearSavedWallet();
  const afterForget = structuredClone(db.records);
  await assert.rejects(advanceWalletGeneration(undefined, baseline), /saved wallet changed/);
  assert.deepEqual(db.records, afterForget);
  const current = await readWalletGeneration();
  const next = await advanceWalletGeneration(undefined, current);
  assert.notEqual(next, current);
});


test("selective recovery drains bounded pages and preserves completion across reload", async (t) => {
  installDb(t);
  const worker = new MemoWorker();
  worker.candidates = Array.from({ length: 505 }, (_, i) => `tx${i}`);
  await restartScanWorker(() => worker as unknown as Worker);
  let active = 0, peak = 0, calls = 0;
  const transport = { kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: async (id: string, signal?: AbortSignal) => {
      assert.ok(signal); active++; peak = Math.max(peak, active); calls++;
      await turn(); active--; return id;
    } };
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport });
  await client.restore("test-only worker fixture", "regtest", 1);
  assert.equal((await client.sync()).memoFetchStatus, "scanning");
  assert.equal(calls, 500);
  assert.equal(peak, 8);
  const reloaded = new MemoWorker(); reloaded.candidates = worker.candidates;
  await restartScanWorker(() => reloaded as unknown as Worker);
  const next = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport });
  assert.equal((await next.sync()).memoFetchStatus, "complete");
  assert.equal(calls, 505, "completed entries are never downloaded again after hydration");
  await next.sync(); assert.equal(calls, 505);
});

test("failed selective checkpoint remains pending and an unchanged-tip retry saves completed memos", async (t) => {
  const db = installDb(t); const worker = new MemoWorker();
  worker.candidates = ["blank"];
  await restartScanWorker(() => worker as unknown as Worker);
  let calls = 0;
  const progress: WasmProgress[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: async (id) => { calls++; return id; },
  } }, event => { progress.push(event); });
  await client.restore("test-only worker fixture", "regtest", 1);
  db.failPutKey = "default";
  await assert.rejects(client.sync(), /quota exceeded/);
  assert.equal(progress.some(event => event.stage === "synced"), false);
  assert.equal((await client.getWallet()).memoFetchStatus, "scanning");
  assert.equal((await client.sync()).memoFetchStatus, "complete");
  assert.equal(progress.filter(event => event.stage === "synced").length, 1);
  assert.equal(calls, 1);
  assert.deepEqual(JSON.parse(new TextDecoder().decode(await readSavedSnapshot())), ["blank"]);
});

test("a memo transport outage stops the pass and recovers at an unchanged tip", async (t) => {
  installDb(t); const worker = new MemoWorker();
  worker.candidates = Array.from({ length: 12 }, (_, i) => `tx${i}`);
  await restartScanWorker(() => worker as unknown as Worker);
  let down = true, calls = 0;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: async (id) => { calls++; if (down) throw new TypeError("fetch failed"); return id; },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  assert.equal((await client.sync()).memoFetchStatus, "unavailable");
  assert.equal(calls, 8, "one concurrent group is enough to detect the outage");
  down = false;
  assert.equal((await client.sync()).memoFetchStatus, "complete");
  assert.equal(calls, 20);
});

test("changing memo policy aborts active selective transport requests", async (t) => {
  installDb(t); const worker = new MemoWorker(); worker.candidates = ["blank"];
  await restartScanWorker(() => worker as unknown as Worker);
  let began!: () => void; const started = new Promise<void>(resolve => { began = resolve; });
  let aborted = false;
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(),
    tx: (_id, signal) => new Promise<string>((_resolve, reject) => {
      signal!.addEventListener("abort", () => { aborted = true; reject(signal!.reason); }, { once: true }); began();
    }),
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  const syncing = client.sync(); await started; client.setMemoFetch("on-demand"); await syncing;
  assert.equal(aborted, true); assert.deepEqual(worker.enhanced, []);
});


test("aligned deposits share Zaino's compact download only after protocol negotiation", async (t) => {
  installDb(t);
  for (const [supported, wasmSupported] of [[true, true], [false, true], [true, false]]) {
    const worker = new MemoWorker(); worker.transparentHeight = 10;
    worker.transparentSupported = wasmSupported!;
    const combined = supported && wasmSupported;
    await restartScanWorker(() => worker as unknown as Worker);
    const calls: string[] = [];
    const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transparentScan: "compact", transport: {
      kind: "mock", label: "mock", tip: async () => 11,
      info: async () => ({ chain: "regtest", transparentCompact: true, protocolVersion: supported ? "v0.5.0" : "v0.4.0" }),
      blocks: async () => { calls.push("shielded"); return new Uint8Array([1]); },
      transparentBlocks: async () => { calls.push("all-pools"); return new Uint8Array([1]); },
      utxos: async () => { throw new Error("must not request an address"); },
    } });
    await client.restore("test-only worker fixture", "regtest", 1);
    const result = await client.sync();
    assert.deepEqual(calls, [combined ? "all-pools" : "shielded"]);
    assert.deepEqual(worker.combinedApplies, [combined]);
    assert.equal(result.transparentScanStatus, combined ? "complete" : "unsupported");
  }
});

test("old local WASM rejects a combined apply before advancing shielded state", async () => {
  let applied = 0;
  const session = localScanSession({
    applyCompactBlocks: () => { applied++; return "[]"; }, scannedHeight: () => applied,
  } as never);
  assert.equal(await session.supportsTransparentBlocks!(), false);
  await assert.rejects(session.applyBlob(new Uint8Array([1]), true), /public-data upgrade/);
  assert.equal(applied, 0);
  await session.applyBlob(new Uint8Array([1]));
  assert.equal(applied, 1);
});

test("disabling deposits while a combined fetch is in flight leaves coverage unchanged", async (t) => {
  installDb(t); const worker = new MemoWorker(); worker.transparentHeight = 10;
  await restartScanWorker(() => worker as unknown as Worker);
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transparentScan: "compact", transport: {
    kind: "mock", label: "mock", tip: async () => 11,
    info: async () => ({ chain: "regtest", transparentCompact: true, protocolVersion: "v0.5.0" }),
    blocks: async () => { throw new Error("already fetched all pools"); },
    transparentBlocks: async () => { client.setTransparentScan("off"); return new Uint8Array([1]); },
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  const result = await client.sync();
  assert.equal(result.scannedHeight, 11);
  assert.equal(result.transparentScanHeight, 10);
  assert.equal(result.transparentScanStatus, "off");
  assert.deepEqual(worker.combinedApplies, [false]);
});

test("a full legacy memo page reports ongoing work rather than a server failure", async (t) => {
  installDb(t); const worker = new MemoWorker(); worker.legacyEnhancement = true;
  worker.candidates = Array.from({ length: 40 }, (_, i) => `tx${i}`);
  await restartScanWorker(() => worker as unknown as Worker);
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => new Uint8Array(), tx: async id => id,
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  assert.equal((await client.sync()).memoFetchStatus, "scanning");
  assert.equal((await client.sync()).memoFetchStatus, "complete");
});

test("combined gRPC scans bound queued ranges independently of worker parallelism", async (t) => {
  installDb(t);
  const worker = new MemoWorker(); worker.scanned = 0; worker.transparentHeight = 0;
  await restartScanWorker(() => worker as unknown as Worker);
  const ranges: Array<[number, number]> = [];
  let began!: () => void; const started = new Promise<void>(resolve => { began = resolve; });
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transparentScan: "compact", transport: {
    kind: "grpc-web", label: "mock", tip: async () => 10_000,
    info: async () => ({ chain: "regtest", transparentCompact: true, protocolVersion: "v0.5.0" }),
    blocks: async () => { throw new Error("must share the all-pool download"); },
    transparentBlocks: (start, end, signal) => new Promise<Uint8Array>((_resolve, reject) => {
      ranges.push([start, end]);
      signal!.addEventListener("abort", () => reject(signal!.reason), { once: true });
      if (ranges.length === 4) began();
    }),
  } });
  await client.restore("test-only worker fixture", "regtest", 1);
  const sync = client.sync();
  await started;
  cancelWasmSync();
  await assert.rejects(sync, /cancel/i);
  assert.deepEqual(ranges, [[1, 500], [501, 1000], [1001, 1500], [1501, 2000]]);
});

function noisySnapshot(size: number): Uint8Array {
  let seed = 0x12345678;
  return Uint8Array.from({ length: size }, () => {
    seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5;
    return seed & 255;
  });
}

function payloadWritten(db: ReturnType<typeof memoryIndexedDb>, from: number): number {
  return db.writes.slice(from).reduce((size, key) => {
    const value = db.records.get(key);
    return size + (value instanceof Uint8Array ? value.length : 0);
  }, 0);
}

function assertNoSnapshotOrphans(db: ReturnType<typeof memoryIndexedDb>) {
  const ref = db.records.get("default") as { key: string; chunkCount: number };
  const expected = new Set(["default"]);
  for (let page = 0; page < Math.ceil(ref.chunkCount / 128); page++) {
    const key = page ? `${ref.key}:i:${page}` : ref.key;
    expected.add(key);
    for (const chunk of db.records.get(key) as { key: string }[]) expected.add(chunk.key);
  }
  assert.deepEqual(new Set(db.records.keys()), expected);
}

test("incremental saves reuse unchanged data across insertion, deletion, append and rollback", async t => {
  const db = installDb(t);
  const original = noisySnapshot(4 * 1024 * 1024);
  let key = await saveWalletSnapshot(original, preview(), () => true);
  const inserted = new Uint8Array(original.length + 133);
  inserted.set(original.subarray(0, 1000));
  inserted.fill(42, 1000, 1133);
  inserted.set(original.subarray(1000), 1133);
  const appended = new Uint8Array(inserted.length + 1000);
  appended.set(inserted); appended.fill(7, inserted.length);
  const shrunk = appended.slice(140_003, -140_003);
  let round = 0;
  for (const bytes of [original, inserted, appended, shrunk, original]) {
    const writes = db.writes.length;
    const previousKey = key;
    key = await saveWalletSnapshot(bytes, preview(20), () => true, undefined, key);
    assert.notEqual(key, previousKey, "even an identical save has a fresh CAS revision");
    const changed = payloadWritten(db, writes);
    assert.ok(changed < 400_000, `a localized edit rewrote ${changed} of ${bytes.length} bytes`);
    if (round++ === 0) assert.equal(changed, 0, "identical bytes only write the bounded index and preview");
    assert.deepEqual(await readSavedSnapshot(), bytes);
    assert.equal((await peekSavedSnapshot(() => null))?.scannedHeight, 20);
    assertNoSnapshotOrphans(db);
  }
});

test("incremental commits repair damaged candidate payloads and preserve old data on aborted commits", async t => {
  const db = installDb(t);
  const bytes = noisySnapshot(512 * 1024);
  let key = await saveWalletSnapshot(bytes, preview(), () => true);
  const chunks = db.records.get(key!) as { key: string; length: number; hash: number }[];
  const damaged = chunks[0].key;
  const missing = chunks[1].key;
  db.records.set(damaged, new Uint8Array(chunks[0].length));
  db.records.delete(missing);
  key = await saveWalletSnapshot(bytes, preview(20), () => true, undefined, key);
  assert.deepEqual(await readSavedSnapshot(), bytes);
  assertNoSnapshotOrphans(db);
  const before = structuredClone(db.records);
  const changed = bytes.slice(); changed[0] ^= 255;
  db.abortNextCommit = true;
  await assert.rejects(saveWalletSnapshot(changed, preview(30), () => true, "initial", key), /aborted/);
  assert.deepEqual(db.records, before, "payload deletion and the replacement index both roll back");
  assert.deepEqual(await readSavedSnapshot(), bytes);
});

test("incremental index corruption fails closed without deleting recovery data", async t => {
  const db = installDb(t);
  const bytes = noisySnapshot(4 * 1024 * 1024);
  const key = await saveWalletSnapshot(bytes, preview(), () => true);
  const pageKey = `${key}:i:1`;
  assert.ok(db.records.has(pageKey), "fixture must use several bounded index pages");
  const page = db.records.get(pageKey);
  for (const damaged of [undefined, [], [{ key: "seed.enc", length: 32, hash: 0 }]]) {
    db.records.set(pageKey, damaged);
    const before = structuredClone(db.records);
    await assert.rejects(readSavedSnapshot(), /snapshot is incomplete/);
    await assert.rejects(saveWalletSnapshot(bytes, preview(20), () => true, undefined, key), /snapshot is incomplete/);
    assert.deepEqual(db.records, before);
  }
  db.records.set(pageKey, page);
  assert.deepEqual(await readSavedSnapshot(), bytes);
});

test("format-2 migration is atomic and preserves old chunks after quota failure", async t => {
  const db = installDb(t);
  const bytes = noisySnapshot(200_000);
  const key = "snapshot:old-format-2";
  db.records.set("default", { format: "z-stack-snapshot-ref-2", key, byteLength: bytes.length, preview: preview() });
  for (let offset = 0, i = 0; offset < bytes.length; offset += 32768, i++) {
    db.records.set(i ? `${key}:${i}` : key, bytes.slice(offset, offset + 32768));
  }
  assert.deepEqual(await readSavedSnapshot(), bytes);
  const before = structuredClone(db.records);
  db.failPutKey = "default";
  await assert.rejects(saveWalletSnapshot(bytes, preview(20), () => true, undefined, key), /quota/);
  assert.deepEqual(db.records, before);
  await saveWalletSnapshot(bytes, preview(20), () => true, undefined, key);
  assert.deepEqual(await readSavedSnapshot(), bytes);
  assertNoSnapshotOrphans(db);
  await assert.rejects(saveWalletSnapshot(bytes, preview(30), () => true, undefined, key),
    (e: unknown) => e instanceof WalletError && e.code === "wallet_changed");
  await clearSavedWallet();
  assert.deepEqual([...db.records.keys()], [WALLET_GENERATION_KEY]);
});

test("cancelling incremental preparation or commit leaves the previous revision intact", async t => {
  const db = installDb(t);
  const bytes = noisySnapshot(4 * 1024 * 1024);
  const key = await saveWalletSnapshot(bytes, preview(), () => true);
  const before = structuredClone(db.records);
  let current = true;
  const preparing = saveWalletSnapshot(bytes, preview(20), () => current, "initial", key);
  await turn();
  current = false;
  abortSnapshotWrites();
  assert.equal(await preparing, undefined);
  assert.deepEqual(db.records, before);
  current = true;
  db.holdCommits = true;
  const committing = saveWalletSnapshot(bytes, preview(30), () => current, "initial", key);
  while (!db.commits.length) await new Promise(resolve => setTimeout(resolve, 5));
  current = false;
  abortSnapshotWrites();
  assert.equal(await committing, undefined);
  db.commits.shift()!();
  db.holdCommits = false;
  assert.deepEqual(db.records, before);
});

test("a replacement generation never retains the old wallet's chunk keys", async t => {
  const db = installDb(t);
  const bytes = noisySnapshot(256 * 1024);
  const oldKey = await saveWalletSnapshot(bytes, preview(), () => true);
  const oldKeys = new Set(db.records.keys());
  const generation = await advanceWalletGeneration();
  await saveWalletSnapshot(bytes, preview(20), () => true, generation, null);
  for (const key of db.records.keys()) {
    if (String(key).startsWith("snapshot:")) assert.ok(!oldKeys.has(key));
  }
  await assert.rejects(saveWalletSnapshot(bytes, preview(30), () => true, "initial", oldKey),
    (e: unknown) => e instanceof WalletError && e.code === "wallet_changed");
  assert.deepEqual(await readSavedSnapshot(), bytes);
});

for (const birthday of [1, 2]) test(`same-tip wallets retry roots without resetting birthday ${birthday} trees`, async t => {
  installDb(t);
  const worker = new MemoWorker(); worker.candidates = []; worker.birthday = birthday;
  attachWasmBindings({} as never);
  await restartScanWorker(() => worker as unknown as Worker);
  await attachScanWorker(worker as unknown as Worker, { threads: 1, preferMulticore: false });
  const requested: string[] = [];
  const client = createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, memoFetch: "on-demand", transport: {
    kind: "mock", label: "mock", tip: async () => 10, blocks: async () => { throw new Error("already at tip"); },
    treeState: async () => { throw new Error("must not reset the frontier"); },
    subtreeRoots: async (pool) => { requested.push(pool); return [{ completingHeight: 5, rootHash: "11".repeat(32) }]; },
  } });
  await client.restore("test-only worker fixture", "regtest", birthday);
  worker.ops.length = 0;
  await client.sync();
  assert.deepEqual(requested, ["sapling", "orchard"]);
  assert.equal(worker.ops.filter(op => op === "applySubtreeRoots").length, 2);
  assert.equal(worker.ops.includes("applyTreeState"), false);
  await client.sync();
  assert.equal(requested.length, 2, "root refresh is bounded within a session");
  const later = Date.now() + 600_001;
  t.mock.method(Date, "now", () => later);
  await client.sync();
  assert.equal(requested.length, 4, "long-lived sessions refresh roots periodically");
  assert.equal(worker.scanned, 10);
  await client.restore("test-only replacement fixture", "regtest", birthday);
  await client.sync();
  assert.equal(requested.length, 6, "replacement reusing a worker gets its own root refresh");
});
