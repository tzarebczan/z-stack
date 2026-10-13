import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { setImmediate as turn } from "node:timers/promises";
import { attachWasmBindings, cancelWasmSync, createWasmClient, forgetWasmWallet, peekWasmWallet } from "../src/wasm-client.ts";
import { attachScanWorker, restartScanWorker, workerScanSession } from "../src/scan-host.ts";
import { clearSavedWallet, readSavedSnapshot } from "../src/snapshot-storage.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => { resolve = r; });
  return { promise, resolve };
};
const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const decode = (bytes: Uint8Array) => JSON.parse(new TextDecoder().decode(bytes));
// The pagehide listener is bound once per document and reads the latest client's callback.
const page = new EventTarget();
const documentEvents = new EventTarget();

function fixture(t: TestContext, configure?: (handle: Record<string, unknown>, name: string) => void) {
  const db = memoryIndexedDb();
  for (const [name, value] of Object.entries({ indexedDB: db.indexedDB, IDBKeyRange: db.IDBKeyRange, window: page, document: documentEvents })) {
    const old = Object.getOwnPropertyDescriptor(globalThis, name);
    Object.defineProperty(globalThis, name, { value, configurable: true });
    t.after(() => old ? Object.defineProperty(globalThis, name, old) : Reflect.deleteProperty(globalThis, name));
  }
  const counts = { created: 0, freed: 0 };
  function wallet(name: string, pending = false) {
    let freed = false;
    const handle = {
      free() { assert.equal(freed, false, "Rust handle freed twice"); freed = true; counts.freed++; },
      scannedHeight: () => 10,
      toSnapshot: () => encode({ name, pending }),
      snapshotJson: () => JSON.stringify({ network: "regtest", birthdayHeight: 1, scannedHeight: 10,
        unifiedAddress: name, ufvk: "uview-fixture", balance: { totalAvailable: pending ? 0 : 10, orchardAvailable: pending ? 0 : 10 },
        transactions: pending ? [{ txid: "fixture", status: "pending" }] : [] }),
      history: () => JSON.stringify(pending ? [{ txid: "fixture", status: "pending" }] : []),
      nextUnifiedAddress() {},
      hardwareReleaseLocks() { return 0; },
      abandon() { pending = false; },
      proveSend() {
        if (pending) throw new Error("inputs already reserved");
        pending = true;
        return JSON.stringify({ hex: "synthetic-transaction", txid: "ab".repeat(32) });
      },
    };
    configure?.(handle, name);
    return handle;
  }
  attachWasmBindings({ generateMnemonic: () => "generated-fixture", WasmWallet: {
    create: (_network: string, name: string) => { counts.created++; return wallet(name); },
    fromUfvk: (_network: string, name: string) => { counts.created++; return wallet(name); },
    fromSnapshot: (bytes: Uint8Array) => { const state = decode(bytes); return wallet(state.name, state.pending); },
  } } as never);
  return { db, counts };
}
function client(tip: () => Promise<number>, submit?: (hex: string) => Promise<string>) {
  return createWasmClient({ unlockPolicy: "session", memoFetch: "auto", network: "regtest", autoShield: false, transport: {
    kind: "mock", label: "mock", tip, blocks: async () => new Uint8Array(), submit,
  } });
}

for (const passphrase of [undefined, "disposable-fixture-passphrase"]) {
  test(`create returns its phrase after close races physical commit (${passphrase ? "backup requested" : "no backup"})`, async (t) => {
    let db!: ReturnType<typeof memoryIndexedDb>;
    ({ db } = fixture(t, handle => {
      const snapshot = handle.snapshotJson as () => string;
      handle.snapshotJson = () => { db.holdCompletionEvents = true; return snapshot(); };
    }));
    const c = client(async () => 10);
    const creating = c.create("regtest", 1, { passphrase });
    await turn();
    assert.equal(db.completions.length, 1, "wallet bytes committed before the completion event");
    assert.equal((db.records.get("default") as { preview: { unifiedAddress: string } }).preview.unifiedAddress, "generated-fixture");
    const closing = c.dispose();
    await turn();
    const committed = structuredClone(db.records);
    db.holdCompletionEvents = false;
    db.completions.shift()!();
    const result = await creating;
    await closing;
    assert.equal(result.mnemonic, "generated-fixture", "durable creation must deliver its recovery phrase");
    assert.equal(result.localSeedBackup, passphrase ? "not-saved" : undefined);
    assert.equal(c.hasSpendingSeed(), false, "closed creation cannot reattach a seed");
    assert.deepEqual(db.records, committed);
    assert.equal(db.records.has("seed.enc"), false);
    assert.equal(decode((await readSavedSnapshot())!).name, "generated-fixture");
    await forgetWasmWallet();
  });
}

test("create cancelled before physical commit rejects and leaves no durable wallet", async (t) => {
  let db!: ReturnType<typeof memoryIndexedDb>;
  ({ db } = fixture(t, handle => {
    const snapshot = handle.snapshotJson as () => string;
    handle.snapshotJson = () => { db.holdCommits = true; return snapshot(); };
  }));
  const c = client(async () => 10);
  const creating = c.create("regtest", 1);
  const rejected = assert.rejects(creating, /abort|cancelled/i);
  await turn();
  assert.equal(db.commits.length, 1);
  assert.equal(db.records.has("default"), false);
  db.holdCommits = false;
  await c.dispose();
  db.commits.shift()!();
  await rejected;
  assert.equal(await readSavedSnapshot(), null);
  assert.equal(c.hasSpendingSeed(), false);
  await forgetWasmWallet();
});

for (const backupFails of [false, true]) {
  test(`create reports optional encrypted backup ${backupFails ? "failure" : "success"} separately`, async (t) => {
    const { db } = fixture(t);
    const c = client(async () => 10);
    if (backupFails) db.failPutKey = "seed.enc";
    const result = await c.create("regtest", 1, { passphrase: "disposable-fixture-passphrase" });
    assert.equal(result.mnemonic, "generated-fixture");
    assert.equal(result.localSeedBackup, backupFails ? "not-saved" : "saved");
    assert.equal(db.records.has("seed.enc"), !backupFails);
    assert.equal(decode((await readSavedSnapshot())!).name, "generated-fixture");
    assert.equal((db.records.get("default") as { preview: Record<string, unknown> }).preview.mnemonic, undefined);
    await forgetWasmWallet();
  });
}

for (const method of ["create", "restore", "restoreUfvk"] as const) {
  test(`${method} waiting for tip cannot resurrect a forgotten wallet`, async (t) => {
    const { db, counts } = fixture(t);
    const tip = deferred<number>();
    const c = client(() => tip.promise);
    const pending = method === "create" ? c.create("regtest", 1)
      : method === "restore" ? c.restore("restore-fixture", "regtest", 1)
        : c.restoreUfvk!("uview-fixture", "regtest", 1);
    const rejected = assert.rejects(pending, /cancelled|AbortError/);
    await forgetWasmWallet();
    tip.resolve(10);
    await rejected;
    assert.equal(counts.created, 0);
    assert.equal(db.records.has("default"), false);
    assert.equal(c.hasSpendingSeed?.(), false);
  });
}

for (const method of ["restore", "restoreUfvk"] as const) {
  test(`${method} saves its explicit unlock policy without caching an each-spend seed`, async (t) => {
    const { db } = fixture(t);
    const values = new Map<string, string>();
    const old = Object.getOwnPropertyDescriptor(globalThis, "sessionStorage");
    Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    } });
    t.after(() => old ? Object.defineProperty(globalThis, "sessionStorage", old) : Reflect.deleteProperty(globalThis, "sessionStorage"));
    const c = client(async () => 10, async () => "fixture-txid");
    const key = method === "restore" ? "restore-fixture" : "uview-fixture";
    const restored = await c[method](key, "regtest", 1, { unlockPolicy: "each-spend" });
    assert.equal(restored.unlockPolicy, "each-spend");
    assert.equal(c.unlockPolicy(), "each-spend");
    assert.deepEqual(db.records.get("meta"), { unlockPolicy: "each-spend" });
    assert.equal((await peekWasmWallet())?.unlockPolicy, "each-spend", "the committed preview carries the same policy");
    assert.equal(values.size, 0, "the phrase must not enter sessionStorage");
    if (method === "restore") {
      await assert.rejects(c.send("recipient", "0.00005"), /re-enter spending seed/);
      assert.equal(c.hasSpendingSeed(), false, "a restore starts locked");
    }
    await forgetWasmWallet();
  });
}

for (const method of ["restore", "restoreUfvk"] as const) {
  for (const failure of ["policy quota", "transaction abort", "sync cancellation"] as const) {
    test(`${method} preserves the prior wallet and policy after ${failure} during initial commit`, async (t) => {
      const { db } = fixture(t);
      const c = client(async () => 10);
      await c.restore("prior-wallet", "regtest", 1, { unlockPolicy: "each-spend" });
      db.records.set("seed.enc", { ciphertext: "existing encrypted seed" });
      db.records.set("passkey.v1", { ciphertext: "existing encrypted passkey" });
      const before = structuredClone(db.records);
      const key = method === "restore" ? "replacement-fixture" : "uview-replacement-fixture";
      if (failure === "policy quota") db.failPutKey = "meta";
      else db.holdCommits = true;
      const restoring = c[method](key, "regtest", 1, { unlockPolicy: "session" });
      const rejected = assert.rejects(restoring, /quota exceeded|aborted|cancelled/);
      if (failure !== "policy quota") {
        await turn();
        assert.equal(db.commits.length, 1, "all replacement write requests completed, but the transaction has not committed");
        const writes = db.writes.length;
        // A real pagehide must not publish the replacement while its required
        // commit is pending; explicit mutations cannot claim to have saved either.
        page.dispatchEvent(new Event("pagehide"));
        await assert.rejects(c.nextAddress(), /restore is still in progress/);
        await assert.rejects(c.resetScan(), /restore is still in progress/);
        await assert.rejects(c.setUnlockPolicy("always"), /restore is still in progress/);
        assert.equal(db.writes.length, writes);
        db.holdCommits = false;
        if (failure === "transaction abort") db.abortNextCommit = true;
        else cancelWasmSync();
        db.commits.shift()!();
      }
      await rejected;
      assert.deepEqual(db.records, before, "snapshot, preview, policy, vaults and generation all remain unchanged");
      assert.equal(c.hasSpendingSeed(), false);
      assert.equal(c.unlockPolicy(), "each-spend");
      assert.equal(decode((await readSavedSnapshot())!).name, "prior-wallet");
      assert.equal((await c.getWallet()).unifiedAddress, "prior-wallet");
      const reloaded = client(async () => 10);
      await turn();
      assert.equal((await reloaded.getWallet()).unifiedAddress, "prior-wallet");
      assert.equal(reloaded.unlockPolicy(), "session", "application policy wins over saved metadata");
      await forgetWasmWallet();
    });
  }
  for (const successor of ["forget", "restore"] as const) {
    test(`${method} cannot roll back ${successor} while its initial commit is pending`, async (t) => {
      const { db } = fixture(t);
      const c = client(async () => 10);
      await c.restore("prior-wallet", "regtest", 1, { unlockPolicy: "each-spend" });
      db.holdCommits = true;
      const restoring = c[method](method === "restore" ? "superseded" : "uview-superseded", "regtest", 1, { unlockPolicy: "session" });
      const rejected = assert.rejects(restoring, /cancelled/);
      await turn();
      assert.equal(db.commits.length, 1);
      db.holdCommits = false;
      if (successor === "forget") await forgetWasmWallet();
      else await c.restore("successor-wallet", "regtest", 1, { unlockPolicy: "always" });
      await rejected;
      const current = structuredClone(db.records);
      db.commits.shift()!(); // A queued old commit cannot revive the superseded data.
      assert.deepEqual(db.records, current);
      if (successor === "forget") {
        assert.equal(await readSavedSnapshot(), null);
        assert.equal(c.hasSpendingSeed(), false);
      } else {
        assert.equal(decode((await readSavedSnapshot())!).name, "successor-wallet");
        assert.deepEqual(db.records.get("meta"), { unlockPolicy: "always" });
        assert.equal(c.hasSpendingSeed(), true);
        await c.nextAddress(); // The superseded gate cannot suppress later writes.
        assert.notDeepEqual(db.records, current);
        await forgetWasmWallet();
      }
    });
  }
}

for (const twoClients of [false, true]) {
  test(`new restore supersedes a delayed restore (${twoClients ? "two clients" : "same client"})`, async (t) => {
    const { db } = fixture(t);
    const firstTip = deferred<number>();
    let calls = 0;
    const a = client(() => ++calls === 1 ? firstTip.promise : Promise.resolve(10));
    const old = a.restore("old-fixture", "regtest", 1, { unlockPolicy: "each-spend" });
    const rejected = assert.rejects(old, /cancelled/);
    const b = twoClients ? client(async () => 10) : a;
    await b.restore("new-fixture", "regtest", 1, { unlockPolicy: "session" });
    firstTip.resolve(10);
    await rejected;
    assert.equal((await b.getWallet()).unifiedAddress, "new-fixture");
    assert.equal(decode((await readSavedSnapshot())!).name, "new-fixture");
    assert.equal(b.unlockPolicy(), "session");
    assert.deepEqual(db.records.get("meta"), { unlockPolicy: "session" });
    await forgetWasmWallet();
  });
}

test("resetScan's UFVK fallback commits its replacement and releases the initial-save gate", async (t) => {
  const { db } = fixture(t);
  t.mock.method(console, "warn", () => {});
  const c = client(async () => 10);
  await c.restore("before-reset", "regtest", 1, { unlockPolicy: "each-spend" });
  const previous = db.records.get("wallet:generation");
  const reset = await c.resetScan(); // Fixture has no resetScan, so it must rebuild from UFVK.
  assert.equal(reset.unifiedAddress, "uview-fixture");
  assert.equal(decode((await readSavedSnapshot())!).name, "uview-fixture");
  assert.notEqual(db.records.get("wallet:generation"), previous);
  assert.deepEqual(db.records.get("meta"), { unlockPolicy: "each-spend" });
  const saved = db.records.get("default");
  await c.nextAddress();
  assert.notDeepEqual(db.records.get("default"), saved, "ordinary saves resume after the first commit");
  await forgetWasmWallet();
});

test("a delayed restore cannot claim over another document's forget tombstone", async (t) => {
  const { counts } = fixture(t);
  const tip = deferred<number>();
  const c = client(() => tip.promise);
  const restoring = c.restore("stale-fixture", "regtest", 1);
  const rejected = assert.rejects(restoring, /saved wallet changed/);
  await turn(); // Capture the durable baseline before the other document clears it.
  await clearSavedWallet(); // Deliberately does not cancel this document's in-memory lifecycle.
  tip.resolve(10);
  await rejected;
  assert.equal(counts.created, 0);
  assert.equal(await readSavedSnapshot(), null);
  await forgetWasmWallet();
});

test("a restore begun during Forget waits for deletion before publishing", async (t) => {
  const { db, counts } = fixture(t);
  const c = client(async () => 10);
  await turn();
  db.holdCommits = true;
  const forgetting = forgetWasmWallet();
  await turn();
  assert.equal(db.commits.length, 1);
  const restoring = c.restore("new-after-forget-fixture", "regtest", 1);
  await turn();
  assert.equal(counts.created, 0);
  db.holdCommits = false;
  db.commits.shift()!();
  await forgetting;
  await restoring;
  assert.equal(decode((await readSavedSnapshot())!).name, "new-after-forget-fixture");
  await forgetWasmWallet();
});

test("unknown broadcast retains a durable pending transaction and reserved inputs", async (t) => {
  fixture(t);
  let broadcasts = 0;
  const c = client(async () => 10, async () => {
    broadcasts++;
    assert.equal(decode((await readSavedSnapshot())!).pending, true, "persist before network side effect");
    throw new Error("response lost after acceptance");
  });
  await c.restore("send-fixture", "regtest", 1);
  await assert.rejects(c.send("recipient", "0.00005"), /broadcast outcome unknown/);
  assert.equal((await c.getWallet()).balance.totalAvailable, 0);
  assert.equal(decode((await readSavedSnapshot())!).pending, true);
  await assert.rejects(c.send("recipient", "0.00005"), /inputs already reserved/);
  assert.equal(broadcasts, 1);
  await forgetWasmWallet();
});

test("sync cancellation cannot bypass durable input reservation before broadcast", async (t) => {
  const { db } = fixture(t);
  let broadcasts = 0;
  const c = client(async () => 10, async () => { broadcasts++; return "fixture"; });
  await c.restore("send-fixture", "regtest", 1);
  db.holdCommits = true;
  const sending = c.send("recipient", "0.00005");
  const rejected = assert.rejects(sending, /snapshot save cancelled/);
  await turn();
  assert.equal(db.commits.length, 1);
  cancelWasmSync();
  db.holdCommits = false;
  db.commits.shift()!();
  await rejected;
  assert.equal(broadcasts, 0);
  assert.equal((await c.getWallet()).balance.totalAvailable, 10);
  assert.equal(decode((await readSavedSnapshot())!).pending, false);
  await forgetWasmWallet();
});

test("late broadcast completion cannot overwrite a replacement wallet", async (t) => {
  fixture(t);
  const broadcast = deferred<string>();
  const started = deferred<void>();
  const c = client(async () => 10, async () => { started.resolve(); return broadcast.promise; });
  await c.restore("old-send-fixture", "regtest", 1);
  const sending = c.send("recipient", "0.00005");
  const rejected = assert.rejects(sending, error => (error as { code?: string; txid?: string }).code === "broadcast_failed"
    && (error as { txid?: string }).txid === "ab".repeat(32));
  await started.promise;
  await c.restore("new-fixture", "regtest", 1);
  broadcast.resolve("accepted");
  await rejected;
  assert.equal(decode((await readSavedSnapshot())!).name, "new-fixture");
  assert.equal((await c.getWallet()).unifiedAddress, "new-fixture");
  await forgetWasmWallet();
});

class WorkerFixture extends EventTarget {
  ops: string[] = [];
  stopped = 0;
  initId = 0;
  constructor(readonly holdInit = false) { super(); }
  postMessage(msg: { id: number; op: string }) {
    this.ops.push(msg.op);
    if (msg.op === "init" && this.holdInit) { this.initId = msg.id; return; }
    queueMicrotask(() => this.reply(msg.id));
  }
  reply(id: number, error?: string) {
    this.dispatchEvent(new MessageEvent("message", { data: { id, error, threads: 1, mode: "single-thread" } }));
  }
  terminate() { this.stopped++; }
}
const workerOptions = { threads: 1, preferMulticore: false };

test("a stale worker session cannot send RPCs to its successor", async (t) => {
  fixture(t); // Forget now requires an available transactional store.
  const a = new WorkerFixture();
  await attachScanWorker(a as unknown as Worker, workerOptions);
  const old = workerScanSession()!;
  const b = new WorkerFixture();
  await restartScanWorker(() => b as unknown as Worker);
  await assert.rejects(old.applyUtxos("{}"), /restarted/);
  assert.deepEqual(b.ops, ["init"]);
  await workerScanSession()!.applyUtxos("{}");
  assert.deepEqual(b.ops, ["init", "applyUtxos"]);
  await forgetWasmWallet();
});

test("concurrent worker startup coalesces and late initialization cannot retire its successor", async (t) => {
  fixture(t); // Keep persisted deletion separate from worker-only cleanup.
  const a = new WorkerFixture(true);
  const first = attachScanWorker(a as unknown as Worker, workerOptions);
  await turn();
  const duplicate = new WorkerFixture();
  const second = attachScanWorker(duplicate as unknown as Worker, workerOptions);
  assert.equal(duplicate.stopped, 1);
  const b = new WorkerFixture();
  await restartScanWorker(() => b as unknown as Worker);
  a.reply(a.initId, "late old init failure");
  assert.equal(await first, null);
  assert.equal(await second, null);
  assert.equal(b.stopped, 0);
  await workerScanSession()!.applyUtxos("{}");
  assert.deepEqual(b.ops, ["init", "applyUtxos"]);
  await forgetWasmWallet();
});

for (const stage of ["tip", "preparation", "snapshot"] as const) {
  test(`restoreUfvk caller cancellation during ${stage} preserves the previous wallet`, async (t) => {
    const { db } = fixture(t);
    let waitingForTip = false;
    const tip = deferred<number>();
    const preparation = deferred<void>();
    const prepared = deferred<void>();
    const c = client(() => waitingForTip ? tip.promise : Promise.resolve(10));
    await c.restore("prior-wallet", "regtest", 1, { unlockPolicy: "each-spend" });
    db.records.set("seed.enc", { ciphertext: "prior encrypted seed" });
    db.records.set("passkey.v1", { ciphertext: "prior encrypted passkey" });
    const before = structuredClone(db.records);
    const cancellation = new AbortController();
    waitingForTip = stage === "tip";
    db.holdCommits = stage === "snapshot";
    let callbackCount = 0;
    const restoring = c.restoreUfvk("uview-cancelled", "regtest", 1, {
      signal: cancellation.signal,
      unlockPolicy: "session",
      beforeCommit: async (snapshot) => {
        callbackCount++;
        assert.equal(snapshot.unifiedAddress, "uview-cancelled");
        assert.deepEqual(await c.history(), []);
        assert.deepEqual(db.records.get("default"), before.get("default"), "dependent preparation runs before durable replacement");
        assert.deepEqual(db.records.get("meta"), before.get("meta"));
        prepared.resolve();
        if (stage === "preparation") await preparation.promise;
      },
    });
    const rejected = assert.rejects(restoring, /caller cancelled|cancelled|AbortError/);
    if (stage === "snapshot") {
      await prepared.promise;
      await turn();
      assert.equal(db.commits.length, 1, "snapshot and generation transaction is pending");
    } else if (stage === "preparation") await prepared.promise;
    db.holdCommits = false;
    cancellation.abort(new DOMException("caller cancelled", "AbortError"));
    preparation.resolve();
    tip.resolve(10);
    await rejected;
    for (const commit of db.commits.splice(0)) commit();
    assert.deepEqual(db.records, before, "prior snapshot, policy, vaults and generation remain byte-identical");
    assert.equal(callbackCount, stage === "tip" ? 0 : 1);
    assert.equal((await c.getWallet()).unifiedAddress, "prior-wallet", "prior saved wallet is readable without a reload");
    assert.equal(c.unlockPolicy(), "each-spend");
    await forgetWasmWallet();
  });
}

test("restoreUfvk rejects a stale caller after precommit preparation without replacing the wallet", async (t) => {
  const { db } = fixture(t);
  const c = client(async () => 10);
  await c.restore("prior-wallet", "regtest", 1);
  const before = structuredClone(db.records);
  const preparation = deferred<void>();
  const prepared = deferred<void>();
  let current = true;
  const restoring = c.restoreUfvk("uview-cancelled", "regtest", 1, {
    assertCurrent() { if (!current) throw new Error("stale caller"); },
    beforeCommit: async () => { prepared.resolve(); await preparation.promise; },
  });
  const rejected = assert.rejects(restoring, /stale caller/);
  await prepared.promise;
  current = false;
  preparation.resolve();
  await rejected;
  assert.deepEqual(db.records, before);
  assert.equal((await c.getWallet()).unifiedAddress, "prior-wallet");
  await forgetWasmWallet();
});

test("restoreUfvk preparation failure cannot replace the saved wallet", async (t) => {
  const { db } = fixture(t);
  const c = client(async () => 10);
  await c.restore("prior-wallet", "regtest", 1);
  const before = structuredClone(db.records);
  await assert.rejects(c.restoreUfvk("uview-cancelled", "regtest", 1, {
    beforeCommit: async () => { throw new Error("dependent storage failed"); },
  }), /dependent storage failed/);
  assert.deepEqual(db.records, before);
  assert.equal((await c.getWallet()).unifiedAddress, "prior-wallet");
  await forgetWasmWallet();
});

test("restoreUfvk rejects an already cancelled caller before touching the selected wallet", async (t) => {
  const { db, counts } = fixture(t);
  const c = client(async () => 10);
  await c.restore("prior-wallet", "regtest", 1);
  const before = structuredClone(db.records);
  const freed = counts.freed;
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(c.restoreUfvk("uview-cancelled", "regtest", 1, { signal: controller.signal }), /abort/i);
  assert.deepEqual(db.records, before);
  assert.equal(counts.freed, freed);
  assert.equal((await c.getWallet()).unifiedAddress, "prior-wallet");
  await forgetWasmWallet();
});

test("a completed restoreUfvk detaches its caller cancellation and guard", async (t) => {
  const { db } = fixture(t);
  const c = client(async () => 10);
  const controller = new AbortController();
  let current = true;
  let preparationCount = 0;
  const snapshot = await c.restoreUfvk("uview-committed", "regtest", 1, {
    signal: controller.signal,
    assertCurrent() { if (!current) throw new Error("screen unmounted"); },
    beforeCommit: async () => { preparationCount++; },
    unlockPolicy: "each-spend",
  });
  assert.equal(snapshot.unifiedAddress, "uview-committed");
  assert.equal(snapshot.viewOnly, true);
  assert.equal(snapshot.unlockPolicy, "each-spend");
  assert.equal(preparationCount, 1);
  const before = structuredClone(db.records);
  current = false;
  controller.abort();
  assert.equal((await c.getWallet()).unifiedAddress, "uview-committed");
  assert.deepEqual(db.records, before);
  await c.nextAddress();
  await forgetWasmWallet();
});

test("cancelling a superseded caller cannot cancel its successor wallet", async (t) => {
  const { db } = fixture(t);
  const c = client(async () => 10);
  await c.restore("prior-wallet", "regtest", 1);
  const controller = new AbortController();
  const preparation = deferred<void>();
  const prepared = deferred<void>();
  const restoring = c.restoreUfvk("uview-superseded", "regtest", 1, {
    signal: controller.signal,
    beforeCommit: async () => { prepared.resolve(); await preparation.promise; },
  });
  const rejected = assert.rejects(restoring, /cancelled/);
  await prepared.promise;
  const successor = c.restore("successor-wallet", "regtest", 1);
  controller.abort();
  preparation.resolve();
  await rejected;
  await successor;
  assert.equal((await c.getWallet()).unifiedAddress, "successor-wallet");
  assert.equal(decode((await readSavedSnapshot())!).name, "successor-wallet");
  assert.ok(db.records.has("default"));
  await forgetWasmWallet();
});

for (const stale of ["abort", "guard"] as const) {
  test(`restoreUfvk treats physical snapshot commit as success when ${stale} arrives before oncomplete`, async (t) => {
    const { db } = fixture(t);
    const c = client(async () => 10);
    await c.restore("prior-wallet", "regtest", 1);
    const controller = new AbortController();
    let current = true;
    const restoring = c.restoreUfvk("uview-committed", "regtest", 1, {
      signal: controller.signal,
      assertCurrent() { if (!current) throw new Error("caller left after commit"); },
      beforeCommit: () => { db.holdCompletionEvents = true; },
      unlockPolicy: "each-spend",
    });
    await turn();
    assert.equal(db.completions.length, 1, "physical snapshot commit precedes its completion event");
    const committed = structuredClone(db.records);
    assert.equal((committed.get("default") as { preview: { unifiedAddress: string } }).preview.unifiedAddress, "uview-committed");
    current = false;
    if (stale === "abort") controller.abort();
    db.holdCompletionEvents = false;
    db.completions.shift()!();
    const restored = await restoring;
    assert.equal(restored.unifiedAddress, "uview-committed", "must not reject and trigger application rollback after durable success");
    assert.deepEqual(db.records, committed);
    assert.equal((await c.getWallet()).unifiedAddress, "uview-committed");
    await forgetWasmWallet();
  });
}

test("a stalled replacement leaves its prior pending wallet readable to a new client", async (t) => {
  const { db } = fixture(t);
  const c = client(async () => 10, async () => { throw new Error("acknowledgement lost"); });
  await c.restore("prior-wallet", "regtest", 1);
  await assert.rejects(c.send("recipient", "0.00005"), /broadcast/);
  const before = structuredClone(db.records);
  const prepared = deferred<void>(), release = deferred<void>();
  const controller = new AbortController();
  const replacing = c.restoreUfvk("uview-new", "regtest", 1, {
    signal: controller.signal,
    beforeCommit: async () => { prepared.resolve(); await release.promise; },
  });
  const rejected = assert.rejects(replacing, /AbortError/);
  await prepared.promise;
  assert.deepEqual(db.records, before, "derivation must not publish or fence the durable wallet");
  const reopened = client(async () => 10);
  assert.equal((await reopened.getWallet()).unifiedAddress, "prior-wallet");
  assert.deepEqual(await reopened.history(), [{ txid: "fixture", status: "pending" }]);
  controller.abort(new DOMException("page discarded", "AbortError"));
  release.resolve();
  await rejected;
  assert.deepEqual(db.records, before);
  await forgetWasmWallet();
});



test("transparent swap capability fails closed for an older WASM build without reserving inputs", async (t) => {
  const { db } = fixture(t);
  let submitted = 0;
  const c = client(async () => 10, async () => { submitted++; return "unused"; });
  await c.restore("transparent-old-build", "regtest", 1);
  const before = structuredClone(db.records);
  assert.equal(await c.supportsTransparentSend(), false);
  await assert.rejects(c.estimateTransparentFee("recipient", "0.1"), /unavailable/);
  await assert.rejects(c.sendTransparent("recipient", "0.1"), /unavailable/);
  assert.deepEqual(db.records, before);
  assert.equal(submitted, 0);
  await forgetWasmWallet();
});

test("transparent swap dispatch reserves the same wallet and retains its pending result", async (t) => {
  const proofs: unknown[][] = [];
  const estimates: unknown[][] = [];
  const { db } = fixture(t, (handle) => {
    handle.proveTransparentSend = (...args: unknown[]) => {
      proofs.push(args);
      return (handle.proveSend as () => string)();
    };
    handle.estimateTransparentFee = (...args: unknown[]) => {
      estimates.push(args);
      return JSON.stringify({ feeZat: 10_000, feeZec: "0.0001" });
    };
  });
  let submitted = 0;
  const c = client(async () => 10, async () => {
    assert.equal(decode((await readSavedSnapshot())!).pending, true, "reservation is durable before broadcast");
    submitted++;
    return "ab".repeat(32);
  });
  await c.restore("transparent-wallet", "regtest", 1);
  assert.equal(await c.supportsTransparentSend(), true);
  const before = structuredClone(db.records);
  assert.equal((await c.estimateTransparentFee("recipient", "0.1")).feeZat, 10_000);
  assert.deepEqual(estimates, [["recipient", "0.1"]]);
  assert.deepEqual(db.records, before, "fee estimates do not publish a reservation");
  const result = await c.sendTransparent("recipient", "0.1", { maxFeeZat: "10000" });
  assert.equal(result.txid, "ab".repeat(32));
  assert.deepEqual(proofs, [["transparent-wallet", "recipient", "0.1", "10000"]]);
  assert.equal(submitted, 1);
  assert.equal(decode((await readSavedSnapshot())!).pending, true);
  await forgetWasmWallet();
});

for (const method of ["supportsTransparentSend", "estimateTransparentFee", "sendTransparent"] as const) {
  test(`${method} cannot adopt a replacement wallet during capability probing`, async (t) => {
    const gate = deferred<void>();
    let hold = false;
    let proved = 0;
    let fees = 0;
    fixture(t, (handle, name) => {
      const snapshot = handle.snapshotJson as () => string;
      handle.snapshotJson = () => hold && name === "prior" ? gate.promise.then(snapshot) : snapshot();
      handle.proveTransparentSend = () => { proved++; return "{}"; };
      handle.estimateTransparentFee = () => { fees++; return JSON.stringify({ feeZat: 10_000 }); };
    });
    let submitted = 0;
    const c = client(async () => 10, async () => { submitted++; return "unused"; });
    await c.restore("prior", "regtest", 1);
    hold = true;
    const pending = method === "supportsTransparentSend" ? c[method]() : c[method]("recipient", "0.1");
    const rejected = assert.rejects(pending, /cancelled|AbortError/);
    await turn();
    await c.restore("replacement", "regtest", 1);
    gate.resolve();
    await rejected;
    assert.equal(proved, 0);
    assert.equal(fees, 0);
    assert.equal(submitted, 0);
    assert.equal((await c.getWallet()).unifiedAddress, "replacement");
    await forgetWasmWallet();
  });
}

test("transparent swap rejects an over-ceiling proof without a reservation or broadcast", async (t) => {
  const { db } = fixture(t, (handle) => {
    handle.estimateTransparentFee = () => JSON.stringify({ feeZat: 10_000 });
    handle.proveTransparentSend = (_seed: string, _to: string, _amount: string, cap: string) => {
      assert.equal(cap, "9999");
      throw new Error("swap transaction fee exceeds the approved maximum");
    };
  });
  let submitted = 0;
  const c = client(async () => 10, async () => { submitted++; return "unused"; });
  await c.restore("capped-wallet", "regtest", 1);
  const before = structuredClone(db.records);
  await assert.rejects(c.sendTransparent("recipient", "0.1", { maxFeeZat: "9999" }), /approved maximum/);
  for (const maxFeeZat of ["1.1", "-1", "1e4", "9007199254740993", "2100000000000001"]) {
    await assert.rejects(c.sendTransparent("recipient", "0.1", { maxFeeZat }), /invalid maximum fee/);
  }
  assert.deepEqual(db.records, before);
  assert.equal(submitted, 0);
  assert.equal((await c.getWallet()).balance.totalAvailable, 10);
  await forgetWasmWallet();
});

test("transparent swap cannot spend a replacement after waiting for the origin lock", async (t) => {
  let proved = 0;
  fixture(t, (handle) => {
    handle.proveTransparentSend = () => { proved++; return "{}"; };
    handle.estimateTransparentFee = () => JSON.stringify({ feeZat: 10_000 });
  });
  const entered = deferred<void>();
  const release = deferred<void>();
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks: {
    request: async (name: string, _options: unknown, run: (lock: unknown) => unknown) => {
      if (name === "z-stack-wallet-spend") { entered.resolve(); await release.promise; }
      return run({ name });
    },
  } } });
  t.after(() => descriptor ? Object.defineProperty(globalThis, "navigator", descriptor) : Reflect.deleteProperty(globalThis, "navigator"));
  let submitted = 0;
  const c = client(async () => 10, async () => { submitted++; return "unused"; });
  await c.restore("reviewed-wallet", "regtest", 1);
  const pending = c.sendTransparent("reviewed-recipient", "0.1", { maxFeeZat: "10000" });
  const rejected = assert.rejects(pending, /cancelled|AbortError/);
  await entered.promise;
  await c.restore("replacement-wallet", "regtest", 1);
  release.resolve();
  await rejected;
  assert.equal(proved, 0);
  assert.equal(submitted, 0);
  assert.equal((await c.getWallet()).unifiedAddress, "replacement-wallet");
  await forgetWasmWallet();
});

for (const cancellation of ["proof", "reservation", "broadcast event"] as const) {
  test(`transparent swap rolls back when the caller cancels during ${cancellation}`, async (t) => {
    let allowed = true;
    let proved = false;
    const { db } = fixture(t, (handle) => {
      handle.estimateTransparentFee = () => JSON.stringify({ feeZat: 10_000 });
      handle.proveTransparentSend = () => {
        proved = true;
        if (cancellation === "proof") allowed = false;
        return (handle.proveSend as () => string)();
      };
    });
    let submitted = 0;
    const c = client(async () => 10, async () => { submitted++; return "unused"; });
    await c.restore("cancellable-wallet", "regtest", 1);
    if (cancellation === "broadcast event") c.on("broadcast", () => { allowed = false; });
    const before = decode((await readSavedSnapshot())!);
    const beforeBroadcast = () => allowed;
    if (cancellation === "reservation") {
      db.holdCommits = true;
    }
    const pending = c.sendTransparent("recipient", "0.1", { maxFeeZat: "10000", beforeBroadcast });
    const rejected = assert.rejects(pending, /cancelled before broadcast/);
    if (cancellation === "reservation") {
      await turn();
      assert.equal(db.commits.length, 1);
      allowed = false;
      db.holdCommits = false;
      db.commits.shift()!();
    }
    await rejected;
    assert.equal(proved, true);
    assert.equal(submitted, 0);
    assert.deepEqual(decode((await readSavedSnapshot())!), before);
    assert.equal((await c.getWallet()).balance.totalAvailable, 10);
    await forgetWasmWallet();
  });
}

for (const cancellation of ["proof", "reservation", "broadcast event"] as const) {
  for (const guard of ["signal", "predicate"] as const) {
    test(`shielding stops ${guard} cancellation during ${cancellation} before submission`, async (t) => {
      const controller = new AbortController();
      let allowed = true;
      const cancel = () => { allowed = false; controller.abort(); };
      let proved = false;
      const { db } = fixture(t, (handle) => {
        handle.transparentAddress = () => undefined;
        handle.proveShield = () => {
          proved = true;
          if (cancellation === "proof") cancel();
          return (handle.proveSend as () => string)();
        };
      });
      let submitted = 0;
      const c = createWasmClient({ unlockPolicy: "session", network: "regtest", autoShield: false, transport: {
        kind: "mock", label: "shield-fixture", tip: async () => 10,
        blocks: async () => new Uint8Array(), utxos: async () => [],
        submit: async () => { submitted++; return "unused"; },
      } });
      await c.restore("shield-fixture", "regtest", 1);
      if (cancellation === "broadcast event") c.on("broadcast", cancel);
      const before = decode((await readSavedSnapshot())!);
      if (cancellation === "reservation") db.holdCommits = true;
      const pending = c.shield(100_000, guard === "signal"
        ? { signal: controller.signal } : { beforeBroadcast: () => allowed });
      const rejected = assert.rejects(pending, /cancelled before broadcast/);
      if (cancellation === "reservation") {
        await turn();
        assert.equal(db.commits.length, 1);
        cancel();
        db.holdCommits = false;
        db.commits.shift()!();
      }
      await rejected;
      assert.equal(proved, true);
      assert.equal(submitted, 0);
      assert.deepEqual(decode((await readSavedSnapshot())!), before);
      await forgetWasmWallet();
    });
  }
}

for (const outcome of ["accepted", "unknown"] as const) {
  test(`shielding cancelled after submission preserves its ${outcome} outcome`, async (t) => {
    fixture(t, (handle) => {
      handle.transparentAddress = () => undefined;
      handle.proveShield = handle.proveSend;
    });
    const started = deferred<void>();
    const finish = deferred<void>();
    const controller = new AbortController();
    let submitted = 0;
    const c = createWasmClient({ unlockPolicy: "session", network: "regtest", autoShield: false, transport: {
      kind: "mock", label: "shield-fixture", tip: async () => 10,
      blocks: async () => new Uint8Array(), utxos: async () => [],
      submit: async () => {
        submitted++;
        started.resolve();
        await finish.promise;
        if (outcome === "unknown") throw new Error("fixture connection lost");
        return "ab".repeat(32);
      },
    } });
    await c.restore("shield-fixture", "regtest", 1);
    const pending = c.shield(100_000, { signal: controller.signal });
    const checked = outcome === "unknown"
      ? assert.rejects(pending, /broadcast outcome unknown/)
      : pending.then(result => assert.equal(result.txid, "ab".repeat(32)));
    await started.promise;
    controller.abort();
    finish.resolve();
    await checked;
    assert.equal(submitted, 1);
    assert.equal(decode((await readSavedSnapshot())!).pending, true);
    await forgetWasmWallet();
  });
}

for (const phase of ["proof", "reservation", "accepted", "unknown"] as const) {
  test(`sync cancellation during automatic shielding ${phase} cannot broadcast again or publish late balance`, async (t) => {
    let db!: ReturnType<typeof memoryIndexedDb>;
    let proved = false;
    let balances = 0;
    let balancesAtCancellation = 0;
    const cancel = () => { balancesAtCancellation = balances; cancelWasmSync(); };
    ({ db } = fixture(t, handle => {
      handle.nextHeight = () => 11;
      handle.birthday = () => 1;
      handle.treesReady = () => true;
      handle.transparentAddress = () => undefined;
      const snapshot = handle.snapshotJson as () => string;
      handle.snapshotJson = () => {
        const wallet = JSON.parse(snapshot());
        wallet.balance.transparentAvailable = wallet.balance.totalAvailable ? 200_000 : 0;
        wallet.balance.totalAvailable = wallet.balance.transparentAvailable;
        wallet.balance.orchardAvailable = 0;
        return JSON.stringify(wallet);
      };
      handle.proveShield = () => {
        proved = true;
        if (phase === "proof") cancel();
        if (phase === "reservation") db.holdCommits = true;
        return (handle.proveSend as () => string)();
      };
    }));
    const started = deferred<void>();
    const finish = deferred<void>();
    let submitted = 0;
    const c = createWasmClient({ unlockPolicy: "session", network: "regtest", autoShield: true,
      prewarmProvingKey: false, memoFetch: "on-demand", transport: {
        kind: "mock", label: "automatic-shield-fixture", tip: async () => 10,
        blocks: async () => new Uint8Array(), utxos: async () => [],
        submit: async () => {
          submitted++;
          started.resolve();
          if (phase === "accepted" || phase === "unknown") await finish.promise;
          if (phase === "unknown") throw new Error("fixture acknowledgement lost");
          return "ab".repeat(32);
        },
      } });
    await c.restore("automatic-shield-fixture", "regtest", 1);
    const before = decode((await readSavedSnapshot())!);
    c.on("balance", () => { balances++; });
    const pending = c.sync();
    const rejected = assert.rejects(pending, /sync cancelled/);
    if (phase === "reservation") {
      for (let i = 0; i < 30 && !db.commits.length; i++) await turn();
      assert.equal(db.commits.length, 1);
      cancel();
      db.holdCommits = false;
      db.commits.shift()!();
    } else if (phase === "accepted" || phase === "unknown") {
      await started.promise;
      cancel();
      finish.resolve();
    }
    await rejected;
    assert.equal(proved, true);
    assert.equal(submitted, phase === "accepted" || phase === "unknown" ? 1 : 0);
    assert.equal(balances, balancesAtCancellation);
    const saved = decode((await readSavedSnapshot())!);
    if (phase === "accepted" || phase === "unknown") assert.equal(saved.pending, true);
    else assert.deepEqual(saved, before);
    await forgetWasmWallet();
  });
}

for (const expired of ["false", "throw"] as const) {
  test(`shielding never rechecks a public authorization hook after accepted submission (${expired})`, async (t) => {
    fixture(t, handle => {
      handle.transparentAddress = () => undefined;
      handle.proveShield = handle.proveSend;
    });
    let submitted = 0;
    let lateAuthorizationChecks = 0;
    let balanceEvents = 0;
    const c = createWasmClient({ unlockPolicy: "session", network: "regtest", autoShield: false, transport: {
      kind: "mock", label: "shield-authority-fixture", tip: async () => 10,
      blocks: async () => new Uint8Array(), utxos: async () => [],
      submit: async () => { submitted++; return "ab".repeat(32); },
    } });
    await c.restore("shield-authority-fixture", "regtest", 1);
    c.on("balance", () => { balanceEvents++; });
    const result = await c.shield(100_000, {
      beforeBroadcast: () => {
        if (!submitted) return true;
        lateAuthorizationChecks++;
        if (expired === "throw") throw new Error("fixture authorization expired");
        return false;
      },
    });
    assert.equal(result.txid, "ab".repeat(32));
    assert.equal(submitted, 1);
    assert.equal(lateAuthorizationChecks, 0);
    assert.equal(balanceEvents, 1);
    assert.equal(decode((await readSavedSnapshot())!).pending, true);
    await forgetWasmWallet();
  });
}

for (const cancellation of ["abort-aware", "late-response"] as const) {
  test(`automatic shielding cancels its UTXO preflight without proving or applying its ${cancellation} response`, async (t) => {
    let proved = 0;
    let applied = 0;
    fixture(t, handle => {
      handle.nextHeight = () => 11;
      handle.birthday = () => 1;
      handle.treesReady = () => true;
      handle.transparentAddress = () => "tm-automatic-shield-fixture";
      handle.applyUtxos = () => { applied++; };
      const snapshot = handle.snapshotJson as () => string;
      handle.snapshotJson = () => {
        const wallet = JSON.parse(snapshot());
        wallet.balance.transparentAvailable = 200_000;
        wallet.balance.totalAvailable = 200_000;
        wallet.balance.orchardAvailable = 0;
        return JSON.stringify(wallet);
      };
      handle.proveShield = () => { proved++; return (handle.proveSend as () => string)(); };
    });
    const started = deferred<AbortSignal | undefined>();
    let firstSignal: AbortSignal | undefined;
    let release = () => {};
    let aborted = false;
    let requests = 0;
    let submitted = 0;
    const c = createWasmClient({ unlockPolicy: "session", network: "regtest", autoShield: true,
      prewarmProvingKey: false, memoFetch: "on-demand", transport: {
        kind: "mock", label: "automatic-shield-utxo-fixture", tip: async () => 10,
        blocks: async () => new Uint8Array(),
        utxos: async (_addresses, _start, signal) => {
          requests++;
          if (requests === 1) { firstSignal = signal; return []; }
          return new Promise<[]>((resolve, reject) => {
            release = () => resolve([]);
            signal?.addEventListener("abort", () => {
              aborted = true;
              if (cancellation === "abort-aware") reject(signal.reason);
            }, { once: true });
            started.resolve(signal);
          });
        },
        submit: async () => { submitted++; return "ab".repeat(32); },
      } });
    await c.restore("automatic-shield-utxo-fixture", "regtest", 1);
    const stopped = new AbortController();
    stopped.abort();
    await assert.rejects(c.shield(100_000, { signal: stopped.signal }), /cancelled before broadcast/);
    assert.equal(requests, 0, "already-aborted shielding must not start the UTXO request");
    const syncing = c.sync();
    const cancelled = assert.rejects(syncing, /sync cancelled/);
    const signal = await started.promise;
    cancelWasmSync();
    await turn();
    if (!aborted || cancellation === "late-response") release();
    await cancelled;
    assert.equal(aborted, true);
    assert.ok(signal);
    assert.equal(firstSignal?.aborted, true, "both post-scan and shield preflight providers must observe cancellation");
    assert.equal(signal.aborted, true);
    assert.equal(proved, 0);
    assert.equal(submitted, 0);
    assert.equal(applied, 1, "only the completed post-scan UTXO refresh may apply");
    await forgetWasmWallet();
  });
}

for (const method of ["create", "restore", "restoreUfvk"] as const) {
  test(`${method} passes its owner signal and stops waiting for an uncooperative tip on close`, async t => {
    const { db, counts } = fixture(t);
    const started = deferred<AbortSignal | undefined>();
    const answer = deferred<number>();
    const c = createWasmClient({ network: "regtest", transport: {
      kind: "custom", label: "custom-only",
      tip: async signal => { started.resolve(signal); return answer.promise; },
      blocks: async () => { throw new Error("unexpected block retrieval"); },
    } });
    const pending = method === "create" ? c.create("regtest")
      : method === "restore" ? c.restore("restore-fixture", "regtest", 1)
        : c.restoreUfvk("uview-fixture", "regtest", 1);
    const rejected = assert.rejects(pending, /cancelled|AbortError/);
    const signal = await started.promise;
    assert.ok(signal);
    await c.dispose();
    await Promise.race([rejected, new Promise<never>((_, reject) => {
      const timer = setTimeout(() => reject(new Error("cancelled tip wait did not settle")), 500);
      timer.unref();
    })]);
    assert.equal(signal.aborted, true);
    assert.equal(counts.created, 0);
    assert.equal(db.records.has("default"), false);
    answer.resolve(10);
    await turn();
    assert.equal(db.records.has("default"), false, "late tip cannot revive closed creation");
    await forgetWasmWallet();
  });
}

for (const phase of ["shield-utxos", "shield-utxos-caller", "selective-memo", "public-info"] as const) {
  test(`close stops waiting for ignored ${phase} responses and prevents late apply`, async t => {
    let applies = 0, proved = 0;
    const { db } = fixture(t, handle => {
      handle.nextHeight = () => 11;
      handle.birthday = () => 1;
      handle.treesReady = () => true;
      handle.recomputePools = () => {};
      handle.pendingRawTxs = () => "[]";
      handle.transparentAddress = () => "fixture-transparent";
      handle.applyUtxos = () => { applies++; return 0; };
      handle.applyTransparentBlocks = () => { applies++; return 0; };
      handle.supportsTransparentBlocks = () => true;
      handle.memoEnhancementTxids = () => JSON.stringify(["ab".repeat(32)]);
      handle.enhanceRawTx = () => { applies++; return 0; };
      handle.proveSend = () => { proved++; throw new Error("unexpected proof"); };
    });
    const started = deferred<AbortSignal | undefined>();
    const response = deferred<never[]>();
    const c = createWasmClient({ network: "regtest", unlockPolicy: "session", autoShield: false,
      transparentScan: phase === "public-info" ? "compact" : "off", memoFetch: phase === "selective-memo" ? "auto" : "on-demand",
      transport: { kind: "fixture", label: "provider-only", tip: async () => 10,
        blocks: async () => new Uint8Array(),
        info: phase === "public-info" ? async signal => { started.resolve(signal); await response.promise; return { chain: "regtest", protocolVersion: "v0.5.0", transparentCompact: true }; } : undefined,
        transparentBlocks: phase === "public-info" ? async () => { throw new Error("unexpected public block retrieval"); } : undefined,
        utxos: phase.startsWith("shield-utxos") ? async (_addresses, _start, signal) => { started.resolve(signal); return response.promise; } : undefined,
        tx: phase === "selective-memo" ? async (_txid, signal) => { started.resolve(signal); await response.promise; return "00"; } : undefined,
      } });
    await c.restore("read-provider-fixture", "regtest", 1);
    const caller = new AbortController();
    const pending = phase.startsWith("shield-utxos") ? c.shield(100_000, phase === "shield-utxos-caller" ? { signal: caller.signal } : undefined) : c.sync();
    const rejected = assert.rejects(pending, /cancelled|abort/i);
    const signal = await started.promise;
    assert.ok(signal);
    const before = structuredClone(db.records);
    const appliesBeforeClose = applies;
    await c.dispose();
    try {
      await Promise.race([rejected, new Promise<never>((_, reject) => setTimeout(() => reject(new Error("closed provider wait did not settle")), 500))]);
    } finally { response.resolve([]); await pending.catch(() => {}); }
    await turn();
    assert.equal(signal.aborted, true);
    assert.equal(caller.signal.aborted, false, "owner close must abort the provider while caller remains live");
    assert.equal(applies, appliesBeforeClose);
    assert.equal(proved, 0);
    assert.deepEqual(db.records, before);
    await forgetWasmWallet();
  });
}

test("sync cancellation releases an ignored rebroadcast wait and preserves its existing pending bytes", async t => {
  const raw = "synthetic-existing-pending-transaction";
  const { db } = fixture(t, (handle, name) => {
    handle.nextHeight = () => 11;
    handle.birthday = () => 1;
    handle.treesReady = () => true;
    handle.recomputePools = () => {};
    handle.pendingRawTxs = () => JSON.stringify([raw]);
    handle.toSnapshot = () => encode({ name, pending: true });
  });
  const started = deferred<AbortSignal | undefined>();
  const response = deferred<string>();
  const submitted: string[] = [];
  const c = createWasmClient({ network: "regtest", unlockPolicy: "session", autoShield: false,
    transparentScan: "off", memoFetch: "on-demand", transport: {
      kind: "fixture", label: "read-provider-fixture", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async (bytes, signal) => {
        submitted.push(bytes); started.resolve(signal); return response.promise;
      },
    } });
  await c.restore("pending-fixture", "regtest", 1);
  const pending = c.sync();
  const rejected = assert.rejects(pending, /cancelled|abort/i);
  const signal = await started.promise;
  assert.ok(signal);
  const before = structuredClone(db.records);
  cancelWasmSync();
  try {
    await Promise.race([rejected, new Promise<never>((_, reject) => setTimeout(() => reject(new Error("rebroadcast wait did not cancel")), 500))]);
  } finally { response.resolve("ab".repeat(32)); await pending.catch(() => {}); }
  await turn();
  assert.equal(signal.aborted, true);
  assert.deepEqual(submitted, [raw]);
  assert.deepEqual(db.records, before);
  assert.equal(decode((await readSavedSnapshot())!).pending, true);
  await c.dispose();
  await forgetWasmWallet();
});

test("an overlap apply failure retires the failed engine and ordinary retry reloads saved state", async t => {
  let failOnce = true;
  let applies = 0;
  const { db, counts } = fixture(t, handle => {
    let poisoned = false;
    handle.nextHeight = () => 11;
    handle.birthday = () => 1;
    handle.treesReady = () => true;
    handle.recomputePools = () => {};
    handle.pendingRawTxs = () => "[]";
    handle.applyCompactBlocksSummary = () => {
      applies++;
      if (poisoned) throw new Error("engine remains poisoned");
      if (failOnce) { failOnce = false; poisoned = true; throw new Error("invalid compact transaction"); }
      return JSON.stringify({ scanned: 10, notesFound: 0, spendsFound: 0 });
    };
  });
  const protobuf = Uint8Array.from([16, 10, 26, 32, ...Array(32).fill(10), 34, 32, ...Array(32).fill(9)]);
  const frame = new Uint8Array(protobuf.length + 4);
  new DataView(frame.buffer).setUint32(0, protobuf.length, false);
  frame.set(protobuf, 4);
  const c = createWasmClient({ network: "regtest", autoShield: false, verifyChainTip: true,
    transparentScan: "off", memoFetch: "on-demand", transport: {
      kind: "fixture", label: "overlap retry fixture", tip: async () => 10, blocks: async () => frame,
    } });
  await c.restore("overlap-fixture", "regtest", 1);
  const before = structuredClone(db.records);
  await assert.rejects(c.sync(), /invalid compact transaction/);
  assert.deepEqual(db.records, before);
  assert.equal(counts.freed, 1);
  const result = await c.sync();
  assert.equal(result.unifiedAddress, "overlap-fixture");
  assert.equal(result.scannedHeight, 10);
  assert.equal(applies, 2);
  await c.dispose();
  await forgetWasmWallet();
});
