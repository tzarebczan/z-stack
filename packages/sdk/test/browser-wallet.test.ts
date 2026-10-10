import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test, before, type TestContext } from "node:test";
import { createWallet, forgetWallet, type Wallet } from "../src/create-wallet.ts";
import { memoryWalletStorage } from "../src/storage.ts";
import { attachWasmBindings } from "../src/wasm-client.ts";
import { initialize, deriveAccount } from "../src/runtime.ts";
import { WalletError } from "@z-stack/core";
import { keystoneSigner } from "../src/hardware.ts";
import { saveWalletSnapshot, readSavedSnapshotRecord, advanceWalletGeneration, clearSavedSnapshot } from "../src/snapshot-storage.ts";
import { attachScanWorker, restartScanWorker } from "../src/scan-host.ts";
import { checkWalletSetup } from "../src/diagnostics.ts";

// Public BIP-39 fixture; no network, real money, environment or private secrets.
const words = [...Array(23).fill("abandon"), "art"].join(" ");
const txid = "ab".repeat(32);
const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const decode = (bytes: Uint8Array) => JSON.parse(new TextDecoder().decode(bytes));
before(async () => initialize({ wasmModule: readFileSync(new URL("../src/generated/z_wasm_bg.wasm", import.meta.url)),
  prewarmProveWorker: false, prewarmProvingKey: false }));

function fixture(t: TestContext) {
  const storage = memoryWalletStorage();
  const ufvk = deriveAccount(words, "regtest").ufvk;
  let serial = 0, frees = 0;
  let onSnapshot: (() => void) | undefined;
  let onRevive: (() => void) | undefined;
  let onRescan: (() => void) | undefined;
  const make = (initial: { address: string; pending: boolean; reserved?: boolean; birthday?: number; scanned?: number; transparentScanHeight?: number | null; memoScanHeight?: number | null }) => {
    const state = { ...initial };
    return {
      free() { frees++; },
      toSnapshot: () => { onSnapshot?.(); return encode(state); },
      snapshotJson: () => JSON.stringify({ network: "regtest", unifiedAddress: state.address, ufvk,
        birthdayHeight: state.birthday ?? 8, scannedHeight: state.scanned ?? 10,
        transparentScanHeight: state.transparentScanHeight ?? null, memoScanHeight: state.memoScanHeight ?? null,
        balance: { totalAvailable: state.pending || state.reserved ? 0 : 10, orchardAvailable: state.pending || state.reserved ? 0 : 10, transparentAvailable: 0 } }),
      scannedHeight: () => state.scanned ?? 10,
      nextHeight: () => (state.scanned ?? 10) + 1,
      birthday: () => state.birthday ?? 8,
      pendingRawTxs: () => JSON.stringify(state.pending ? ["synthetic-transaction"] : []),
      rescanFrom(height: number) {
        state.birthday = height; state.scanned = height - 1;
        state.transparentScanHeight = null; state.memoScanHeight = null;
        onRescan?.();
      },
      applyTransparentBlocks(bytes: Uint8Array) {
        if (bytes.length) state.transparentScanHeight = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0);
        return 0;
      },
      applySharedMemos(json: string) { state.memoScanHeight = JSON.parse(json).end; return 0; },
      memoEnhancementTxids: () => "[]",
      history: () => JSON.stringify(state.pending ? [{ txid, status: "pending" }] : []),
      nextUnifiedAddress() {},
      attachSeed(secret: string) { if (secret === "not a phrase") throw new Error("invalid mnemonic: SYNTHETIC_PRIVATE_WORD"); if (secret !== words) throw new Error("seed fingerprint does not match"); },
      proveSend() { state.pending = true; return JSON.stringify({ hex: "synthetic-transaction", txid }); },
      hardwareCreateSend() { state.reserved = true; return new Uint8Array([1]); },
      ledgerSigningPlan() { return JSON.stringify({ commands: [], reviewIndex: 0, signatures: [] }); },
      ledgerApplyResponses(bytes: Uint8Array) { return bytes; },
      hardwareFinalize() { state.pending = true; return JSON.stringify({ hex: "synthetic-hardware-transaction", txid }); },
      hardwareReleaseLocks() { state.reserved = false; return 0; },
      abandon() { state.pending = false; state.reserved = false; return true; },
    };
  };
  attachWasmBindings({ generateMnemonic: () => words, WasmWallet: {
    create: () => make({ address: `fixture-${++serial}`, pending: false }),
    fromSnapshot: (bytes: Uint8Array) => { onRevive?.(); return make(decode(bytes)); },
    hardwareProve: (bytes: Uint8Array) => bytes,
    hardwareSignerCopy: (bytes: Uint8Array) => bytes,
    hardwareCombine: (bytes: Uint8Array) => bytes,
  } } as never);
  let wallet: Wallet | undefined;
  t.after(async () => { await wallet?.close(); });
  return {
    storage,
    frees: () => frees,
    onSnapshot(callback: () => void) { onSnapshot = callback; },
    onRevive(callback?: () => void) { onRevive = callback; },
    onRescan(callback?: () => void) { onRescan = callback; },
    async open(extra: Partial<Parameters<typeof createWallet>[0]> = {}) {
      wallet = await createWallet({ network: "regtest", storage, prewarmProvingKey: false,
        server: { kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(), submit: async () => txid }, ...extra });
      return wallet;
    },
  };
}
const code = (expected: string) => (error: unknown) => error instanceof WalletError && error.code === expected;

test("runtime subscriptions replay, update on scanner readiness and release on close", async t => {
  const f = fixture(t), wallet = await f.open();
  const states: string[] = [];
  const off = wallet.on("runtime", value => states.push(`${value.scanner}:${value.mode}`));
  assert.equal(states.length, 1, "current state is replayed without a click or timer");
  class StartingWorker extends EventTarget {
    id = 0;
    postMessage(msg: { id: number; op: string }) { if (msg.op === "init") this.id = msg.id; }
    terminate() {}
    ready() { this.dispatchEvent(new MessageEvent("message", { data: { id: this.id, mode: "multi-thread", threads: 2 } })); }
  }
  const worker = new StartingWorker();
  const attaching = attachScanWorker(worker as unknown as Worker, { threads: 2, preferMulticore: true });
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(states.at(-1)?.startsWith("starting:"));
  worker.ready(); await attaching;
  assert.equal(states.at(-1), "ready:multi-thread");
  const count = states.length;
  const offSecond = wallet.on("runtime", () => {});
  assert.equal(states.length, count, "a new subscriber does not replay into existing subscribers");
  offSecond(); off();
  await wallet.close();
  assert.equal(states.length, count, "teardown does not notify released handlers");
});

test("startup observers are owned by the wallet and are released after failure or close", async t => {
  const { reportEngineProgress } = await import("../src/engine-progress.ts");
  const f = fixture(t);
  let calls = 0;
  const wallet = await f.open({ onLoadProgress: () => { calls++; } });
  assert.ok(calls > 0);
  await wallet.close();
  const count = calls;
  reportEngineProgress({ component: "keys", phase: "ready" });
  assert.equal(calls, count);
  await assert.rejects(createWallet({ network: "regtest", server: "https://example.invalid", onLoadProgress: () => { calls++; } }), code("wallet_db"));
  reportEngineProgress({ component: "keys", phase: "ready" });
  assert.equal(calls, count);
});

test("public load recovers an interrupted replacement and retains an uncertain payment", async t => {
  const f = fixture(t);
  const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
    blocks: async () => new Uint8Array(), submit: async () => { throw new Error("acknowledgement lost"); } } });
  const created = await wallet.create({ birthday: 1 });
  await wallet.unlock(words);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
  const prior = await readSavedSnapshotRecord();
  await wallet.close();
  // Earlier releases could leave this claim after a page kill without new bytes.
  await f.storage.transaction("readwrite", tx => tx.put("wallet:generation", crypto.randomUUID()));
  const reopened = await f.open();
  assert.equal((await reopened.load())?.unifiedAddress, created.wallet.unifiedAddress);
  assert.deepEqual(await reopened.history(), [{ txid, status: "pending" }]);
  assert.deepEqual(await readSavedSnapshotRecord(), prior);
  await assert.rejects(reopened.create({ birthday: 1 }), code("already_exists"));
});

for (const unlockPolicy of ["each-spend", "session"] as const) {
  test(`public create returns the phrase when close follows durable commit (${unlockPolicy})`, async t => {
    const f = fixture(t);
    let committed!: () => void, release!: () => void;
    const saved = new Promise<void>(resolve => { committed = resolve; });
    const completion = new Promise<void>(resolve => { release = resolve; });
    let hold = true;
    const storage: typeof f.storage = {
      available: true,
      async transaction(mode, body, options) {
        let createsSnapshot = false;
        const value = await f.storage.transaction(mode, tx => body({
          ...tx,
          put(key, data) { if (key === "default") createsSnapshot = true; tx.put(key, data); },
        }), options);
        if (createsSnapshot && hold) {
          hold = false;
          assert.equal(options?.commitWinsCancellation, true);
          committed();
          await completion;
        }
        return value;
      },
    };
    const wallet = await f.open({ storage, unlockPolicy });
    const creating = wallet.create({ birthday: 1 });
    await saved;
    const closing = wallet.close();
    await new Promise(resolve => setTimeout(resolve, 0));
    release();
    const result = await creating;
    await closing;
    assert.equal(result.recoveryPhrase, words);
    assert.equal(result.wallet.mnemonic, undefined);
    assert.throws(() => wallet.hasSpendingSeed(), code("closed"));
    const reopened = await f.open({ storage, unlockPolicy });
    assert.equal((await reopened.load())?.unifiedAddress, result.wallet.unifiedAddress);
    assert.equal(reopened.hasSpendingSeed(), false);
    await assert.rejects(reopened.create({ birthday: 1 }), code("already_exists"));
  });
}

test("lightweight diagnostics observe the public owner across close and reopen", async t => {
  const old = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", { configurable: true, value: {} });
  t.after(() => old ? Object.defineProperty(globalThis, "window", old) : Reflect.deleteProperty(globalThis, "window"));
  const ownerStatus = async () => (await checkWalletSetup()).checks.find(check => check.code === "wallet_owner")?.status;
  const f = fixture(t);
  assert.equal(await ownerStatus(), "pass");
  const wallet = await f.open();
  assert.equal(await ownerStatus(), "info");
  await wallet.close();
  assert.equal(await ownerStatus(), "pass");
  const next = await f.open();
  await wallet.close();
  assert.equal(await ownerStatus(), "info", "a cached old close released the new owner");
  await next.close();
  assert.equal(await ownerStatus(), "pass");
});

test("close releases the owner and session even when hardware rollback cannot be saved", { timeout: 5000 }, async t => {
  const f = fixture(t);
  const failure = new WalletError("wallet_db", "Disposable rollback storage failure.");
  let failWrites = false, failedWrites = 0, submissions = 0;
  const storage: typeof f.storage = {
    available: true,
    transaction: (mode, body, options) => {
      if (failWrites && mode === "readwrite") { failedWrites++; return Promise.reject(failure); }
      return f.storage.transaction(mode, body, options);
    },
  };
  let entered!: () => void, release!: (bytes: Uint8Array) => void;
  const reviewing = new Promise<void>(resolve => { entered = resolve; });
  const signed = new Promise<Uint8Array>(resolve => { release = resolve; });
  const wallet = await f.open({ storage, server: { kind: "fixture", label: "offline", tip: async () => 10,
    blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
  const initial = await wallet.create({ birthday: 1 });
  const rejected = assert.rejects(wallet.send("fixture", "0.00005", undefined,
    { signer: keystoneSigner(async () => { entered(); return signed; }) }), error => error === failure);
  await reviewing;
  failWrites = true;
  await assert.rejects(wallet.close(), error => error === failure);
  await rejected;
  assert.ok(failedWrites > 0);
  assert.ok(f.frees() > 0, "failed rollback left the old session alive");
  await assert.rejects(wallet.getWallet(), code("closed"));
  const reopened = await f.open();
  assert.equal((await reopened.load())?.unifiedAddress, initial.wallet.unifiedAddress);
  assert.equal((await reopened.getWallet()).balance?.totalAvailable, 10);
  release(new Uint8Array([1]));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(submissions, 0);
  // A second close on the retired client must not dispose the new owner's worker.
  await assert.rejects(wallet.close(), error => error === failure);
  assert.equal((await reopened.getWallet()).balance?.totalAvailable, 10);
});

for (const device of ["keystone", "ledger", "ledger-version"] as const) for (const action of ["close", "cancel"] as const) {
  test(`${device} stalled review can ${action}, roll back and ignore a late signature`, { timeout: 5000 }, async t => {
    const f = fixture(t);
    let entered!: () => void;
    let release!: (bytes: Uint8Array) => void;
    const reviewing = new Promise<void>(resolve => { entered = resolve; });
    const response = new Promise<Uint8Array>(resolve => { release = resolve; });
    const signer = device === "keystone" ? keystoneSigner(async () => { entered(); return response; }) : {
      device: "ledger" as const, appVersion: async () => {
        if (device === "ledger-version") { entered(); await response; }
        return "1.0.0";
      },
      exchange: async () => { entered(); return [await response]; },
    };
    let submissions = 0;
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
    await wallet.create({ birthday: 1 });
    const controller = new AbortController();
    const rejected = assert.rejects(wallet.send("fixture", "0.00005", undefined,
      { signer, signal: controller.signal }), code("cancelled"));
    await reviewing;
    if (action === "close") await wallet.close();
    else controller.abort();
    await rejected;
    if (action === "cancel") await wallet.close();
    const reopened = await f.open();
    assert.equal((await reopened.load())?.balance?.totalAvailable, 10);
    assert.deepEqual(await reopened.history(), []);
    // The old external promise may resolve after a replacement owns the engine.
    release(new Uint8Array([1]));
    await new Promise(resolve => setTimeout(resolve, 0));
    assert.equal(submissions, 0);
    assert.equal((await reopened.getWallet()).balance?.totalAvailable, 10);
    await assert.rejects(reopened.send("fixture", "0.00005", undefined,
      { signer: keystoneSigner(async bytes => bytes), beforeBroadcast: () => false }), code("cancelled"));
  });
}

test("failed close preserves an already committed reservation for inspection on reopen", async t => {
  const f = fixture(t);
  const failure = new WalletError("wallet_db", "Disposable rollback storage failure.");
  let failWrites = false, submissions = 0;
  const storage: typeof f.storage = {
    available: true,
    transaction: (mode, body, options) => failWrites && mode === "readwrite"
      ? Promise.reject(failure) : f.storage.transaction(mode, body, options),
  };
  const wallet = await f.open({ storage, server: { kind: "fixture", label: "offline", tip: async () => 10,
    blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
  await wallet.create({ birthday: 1 });
  await wallet.unlock(words);
  let closing!: Promise<void>;
  wallet.on("broadcast", () => {
    failWrites = true;
    closing = wallet.close();
    void closing.catch(() => {});
  });
  await assert.rejects(wallet.send("fixture", "0.00005"), error => error === failure);
  await assert.rejects(closing, error => error === failure);
  assert.equal(submissions, 0);
  const reopened = await f.open();
  // The failed rollback changed no durable bytes. Do not claim its reservation
  // was removed or make a replacement payment automatically.
  assert.equal((await reopened.load())?.balance?.totalAvailable, 0);
  assert.deepEqual((await reopened.history()).map(entry => entry.txid), [txid]);
  assert.equal(reopened.hasSpendingSeed(), false);
});

test("default browser wallet is account-free, memory-locked and never persists a plaintext seed", async t => {
  const f = fixture(t);
  const values = new Map([["z-stack.wasm.session-seed", "obsolete plaintext"]]);
  const original = Object.getOwnPropertyDescriptor(globalThis, "sessionStorage");
  Object.defineProperty(globalThis, "sessionStorage", { configurable: true, value: {
    getItem: (key: string) => values.get(key),
    removeItem: (key: string) => values.delete(key),
    setItem() { assert.fail("plaintext seed write"); },
  } });
  t.after(() => original ? Object.defineProperty(globalThis, "sessionStorage", original) : Reflect.deleteProperty(globalThis, "sessionStorage"));
  await f.storage.transaction("readwrite", tx => tx.put("meta", { unlockPolicy: "always" }));
  const wallet = await f.open();
  const created = await wallet.create({ birthday: 1 });
  assert.equal(created.recoveryPhrase, words);
  assert.equal("mnemonic" in created.wallet, false);
  assert.equal((await wallet.getWallet()).mnemonic, undefined);
  assert.equal(wallet.unlockPolicy(), "each-spend");
  assert.equal(wallet.hasSpendingSeed(), false);
  assert.equal(values.size, 0);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("seed_locked"));
  await wallet.unlock(words);
  assert.equal((await wallet.send("fixture", "0.00005")).txid, txid);
  assert.equal(wallet.hasSpendingSeed(), false);
});

test("replacement requires consent, close releases handles but preserves data, and a second owner is refused", async t => {
  const f = fixture(t);
  const wallet = await f.open();
  const initial = await wallet.create({ birthday: 1 });
  await assert.rejects(wallet.create({ birthday: 1 }), code("already_exists"));
  assert.equal((await wallet.getWallet()).unifiedAddress, initial.wallet.unifiedAddress);
  await assert.rejects(createWallet({ network: "regtest", server: "https://unused.invalid" }), code("busy"));
  await wallet.create({ birthday: 1, replace: true });
  const address = (await wallet.getWallet()).unifiedAddress;
  await wallet.unlock(words);
  await wallet.close();
  await wallet.close();
  assert.ok(f.frees() >= 2);
  await assert.rejects(wallet.getWallet(), code("closed"));
  const reopened = await f.open();
  assert.equal((await reopened.load())?.unifiedAddress, address);
  assert.equal(reopened.hasSpendingSeed(), false);
});

test("late provider unlock cannot survive lock or attach to a replacement wallet", async t => {
  const f = fixture(t);
  let release!: (words: string) => void;
  const pending = new Promise<string>(resolve => { release = resolve; });
  const wallet = await f.open({ unlocker: { unlock: async () => pending } });
  await wallet.create({ birthday: 1 });
  const unlocking = wallet.unlock();
  wallet.lock();
  release(words);
  await assert.rejects(unlocking, code("cancelled"));
  assert.equal(wallet.hasSpendingSeed(), false);
});

for (const signing of ["software", "hardware"] as const) for (const action of ["replace", "forget", "close"] as const) {
  test(`${signing} submission receipt survives ${action} while an acknowledgement is in flight`, async t => {
    const f = fixture(t);
    let release!: (txid: string) => void, entered!: () => void;
    const pending = new Promise<string>(resolve => { release = resolve; });
    const started = new Promise<void>(resolve => { entered = resolve; });
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { entered(); return pending; } } });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    const sending = wallet.send("fixture", "0.00005", undefined,
      signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : undefined);
    const rejected = assert.rejects(sending, error => code("broadcast_failed")(error) && (error as WalletError).txid === txid);
    await started;
    if (action === "replace") await wallet.create({ birthday: 1, replace: true });
    if (action === "forget") await wallet.forget();
    if (action === "close") await wallet.close();
    // Close/replacement must settle even if the provider never acknowledges.
    await rejected;
    release(txid);
    if (action === "close") {
      const reopened = await f.open();
      await reopened.load();
      assert.equal((await reopened.history())[0]?.txid, txid);
      if (signing === "software") await reopened.unlock(words);
      // Proves the abandoned wait released the origin spend mutex without
      // issuing another payment or clearing the pending receipt.
      await assert.rejects(reopened.send("fixture", "0.00005", undefined, {
        beforeBroadcast: () => false,
        ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
      }), code("cancelled"));
    }
  });
}

for (const signing of ["software", "hardware"] as const) {
  test(`${signing} close drains a rollback save already waiting on storage`, { timeout: 5000 }, async t => {
    const f = fixture(t);
    let allowBroadcast = true, holdRollback = false, submissions = 0;
    let entered!: () => void, release!: () => void;
    const waiting = new Promise<void>(resolve => { entered = resolve; });
    const released = new Promise<void>(resolve => { release = resolve; });
    const storage: typeof f.storage = {
      available: true,
      async transaction(mode, body, options) {
        if (holdRollback && mode === "readwrite") {
          holdRollback = false;
          entered();
          await released;
          assert.equal(options?.signal?.aborted, false, "close cancelled the mandatory rollback write");
        }
        return f.storage.transaction(mode, body, options);
      },
    };
    const server = { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } };
    const wallet = await f.open({ storage, server });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    wallet.on("broadcast", () => { allowBroadcast = false; holdRollback = true; });
    const rejected = assert.rejects(wallet.send("fixture", "0.00005", undefined, {
      beforeBroadcast: () => allowBroadcast,
      ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
    }), code("cancelled"));
    await waiting;
    let closed = false;
    const closing = wallet.close().then(() => { closed = true; });
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(closed, false, "close acknowledged before the rollback became durable");
    release();
    await rejected;
    await closing;
    const reopened = await f.open({ storage, server });
    await reopened.load();
    const record = await readSavedSnapshotRecord();
    assert.ok(record);
    assert.equal(decode(record.bytes).pending, false, "reopening retained a cancelled signed transaction");
    assert.deepEqual(await reopened.history(), []);
    assert.equal((await reopened.getWallet()).balance?.totalAvailable, 10);
    assert.equal(submissions, 0);
  });

  test(`${signing} close from broadcast rolls back the unsubmitted reservation before reopening`, async t => {
    const f = fixture(t);
    let submissions = 0;
    let closing: Promise<void> | undefined;
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    wallet.on("broadcast", () => { closing = wallet.close(); });
    await assert.rejects(wallet.send("fixture", "0.00005", undefined,
      signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : undefined), code("cancelled"));
    assert.ok(closing);
    await closing;
    const reopened = await f.open();
    assert.equal(submissions, 0);
    assert.deepEqual(await reopened.history(), []);
    assert.equal((await reopened.load())?.balance?.totalAvailable, 10);
  });

  test(`${signing} send honors an expired review before transport submission`, async t => {
    const f = fixture(t);
    let submissions = 0;
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005", undefined, {
      beforeBroadcast: () => false,
      ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
    }), code("cancelled"));
    assert.equal(submissions, 0);
    assert.equal(wallet.hasSpendingSeed(), false);
  });
}

test("memory storage serializes transactions, clones values, and rolls back a failed write", async () => {
  const storage = memoryWalletStorage();
  const value = { count: 0 };
  await storage.transaction("readwrite", tx => tx.put("counter", value));
  value.count = 999;
  await Promise.all(Array.from({ length: 10 }, () => storage.transaction("readwrite", tx => {
    tx.get("counter", value => tx.put("counter", { count: (value as { count: number }).count + 1 }));
  })));
  await assert.rejects(storage.transaction("readwrite", tx => {
    tx.put("counter", { count: 100 });
    tx.fail(new Error("simulated commit failure"));
  }), /simulated commit failure/);
  const read = () => storage.transaction<{ count: number }>("readonly", tx => tx.get("counter", value => tx.result(value as { count: number })));
  const retrieved = await read();
  assert.equal(retrieved.count, 10);
  retrieved.count = 555;
  assert.equal((await read()).count, 10);
});

test("forget rejects unavailable storage without claiming deletion or retaining the owner lease", async t => {
  const f = fixture(t); let available = true;
  const adapter = { ...f.storage, get available() { return available; } };
  const wallet = await f.open({ storage: adapter });
  const created = await wallet.create({ birthday: 1 });
  await f.storage.transaction("readwrite", tx => {
    tx.put("seed.enc", { ciphertext: "fixture" });
    tx.put("passkey.v1", { ciphertext: "fixture" });
    tx.put("passkey.vault.v1", { ciphertext: "fixture" });
  });
  available = false;
  await assert.rejects(wallet.forget({ passkey: true }), code("wallet_db"));
  await wallet.close();
  await assert.rejects(forgetWallet({ storage: adapter, passkey: true }), code("wallet_db"));
  for (const key of ["seed.enc", "passkey.v1", "passkey.vault.v1"]) {
    assert.deepEqual(await f.storage.transaction("readonly", tx => tx.get(key, value => tx.result(value))), { ciphertext: "fixture" });
  }
  available = true;
  const reopened = await f.open({ storage: adapter });
  assert.equal((await reopened.load())?.unifiedAddress, created.wallet.unifiedAddress);
  await reopened.close();
  await forgetWallet({ storage: adapter, passkey: true });
  for (const key of ["seed.enc", "passkey.v1", "passkey.vault.v1"]) {
    assert.equal(await f.storage.transaction("readonly", tx => tx.get(key, value => tx.result(value))), undefined);
  }
  assert.equal(await (await f.open()).load(), null);
});


test("creation preparation confirms recovery before the first durable snapshot", async t => {
  const f = fixture(t);
  const wallet = await f.open();
  let enter!: () => void, release!: () => void;
  const preparing = new Promise<void>(resolve => { enter = resolve; });
  const confirmed = new Promise<void>(resolve => { release = resolve; });
  const creating = wallet.create({ birthday: 1, beforeCommit: async preparation => {
    assert.equal(preparation.recoveryPhrase, words);
    assert.equal(preparation.signal.aborted, false);
    assert.equal(await f.storage.transaction("readonly", tx => tx.get("default", value => tx.result(value))), undefined);
    preparation.wallet.unifiedAddress = "app-mutation";
    enter(); await confirmed;
  } });
  await preparing;
  assert.equal(await f.storage.transaction("readonly", tx => tx.get("default", value => tx.result(value))), undefined);
  release();
  const created = await creating;
  assert.notEqual(created.wallet.unifiedAddress, "app-mutation");
  await wallet.close();
  assert.equal((await (await f.open()).load())?.unifiedAddress, created.wallet.unifiedAddress);
});

test("close cancels even an uncooperative recovery confirmation without committing a wallet", { timeout: 5000 }, async t => {
  const f = fixture(t);
  const wallet = await f.open();
  let enter!: () => void, release!: () => void, signal!: AbortSignal;
  const preparing = new Promise<void>(resolve => { enter = resolve; });
  const confirmation = new Promise<void>(resolve => { release = resolve; });
  const creating = wallet.create({ birthday: 1, beforeCommit: preparation => {
    signal = preparation.signal; enter(); return confirmation;
  } });
  const rejected = assert.rejects(creating, code("cancelled"));
  await preparing;
  await wallet.close(); await rejected;
  assert.equal(signal.aborted, true);
  release(); await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(await f.storage.transaction("readonly", tx => tx.get("default", value => tx.result(value))), undefined);
  assert.equal(await (await f.open()).load(), null);
});

test("rejected recovery preparation preserves the prior durable wallet during replacement", async t => {
  const f = fixture(t);
  const wallet = await f.open();
  const original = await wallet.create({ birthday: 1 });
  const failure = new Error("App-owned recovery setup failed");
  await assert.rejects(wallet.create({ birthday: 1, replace: true, beforeCommit: () => { throw failure; } }), error => error instanceof WalletError && error.cause === failure);
  await wallet.close();
  assert.equal((await (await f.open()).load())?.unifiedAddress, original.wallet.unifiedAddress);
});

for (const signing of ["software", "hardware"] as const) for (const loss of ["before-proof", "during-serialization"] as const) {
  test(`${signing} never submits after storage becomes unavailable ${loss}`, async t => {
    const f = fixture(t);
    let available = true, submissions = 0;
    const storage: typeof f.storage = { get available() { return available; },
      transaction: (...args) => f.storage.transaction(...args) };
    const wallet = await f.open({ storage, server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
    const initial = await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    if (loss === "during-serialization") f.onSnapshot(() => { available = false; });
    await assert.rejects(wallet.send("fixture", "0.00005", undefined, {
      beforeBroadcast: () => { if (loss === "before-proof") available = false; return true; },
      ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
    }), code("wallet_db"));
    assert.equal(submissions, 0);
    assert.equal(wallet.hasSpendingSeed(), false);
    available = true; f.onSnapshot(() => {});
    // A failed rollback is surfaced, but the owner is still retired.
    await wallet.close().catch(error => { assert.ok(code("wallet_db")(error)); });
    const reopened = await f.open({ storage });
    assert.equal((await reopened.load())?.unifiedAddress, initial.wallet.unifiedAddress);
    assert.equal((await reopened.getWallet()).balance?.totalAvailable, 10);
    assert.deepEqual(await reopened.history(), []);
  });
}


test("recovery confirmation cannot claim a durable replacement generation before acknowledgment", async t => {
  const f = fixture(t);
  const wallet = await f.open();
  const original = await wallet.create({ birthday: 1 });
  const generation = () => f.storage.transaction("readonly", tx => tx.get("wallet:generation", value => tx.result(value)));
  const priorGeneration = await generation();
  let enter!: () => void;
  const preparing = new Promise<void>(resolve => { enter = resolve; });
  const creating = wallet.create({ birthday: 1, replace: true, beforeCommit: () => {
    enter(); return new Promise<void>(() => {});
  } });
  const rejected = assert.rejects(creating, code("cancelled"));
  await preparing;
  assert.equal(await generation(), priorGeneration);
  await wallet.close(); await rejected;
  assert.equal((await (await f.open()).load())?.unifiedAddress, original.wallet.unifiedAddress);
});


for (const unlockPolicy of ["each-spend", "session"] as const) for (const { signing, stalled } of [
  { signing: "software", stalled: "prove" },
  { signing: "hardware", stalled: "prove" },
  { signing: "hardware", stalled: "signerCopy" },
  { signing: "hardware", stalled: "acknowledgement" },
] as const) {
  test(`closing a stalled multicore ${signing} ${stalled} proof releases the public owner (${unlockPolicy})`, { timeout: 5000 }, async t => {
    const f = fixture(t);
    const ufvk = deriveAccount(words, "regtest").ufvk;
    class ProofWorker extends EventTarget {
      state: { address: string; pending: boolean; reserved?: boolean } = { address: "multicore-fixture", pending: false };
      terminated = 0;
      proofId: number | undefined;
      notifyProof!: () => void;
      started = new Promise<void>(resolve => { this.notifyProof = resolve; });
      postMessage(message: { id: number; op: string; snapshot?: ArrayBuffer; action?: string }) {
        const preview = () => ({ network: "regtest", unifiedAddress: this.state.address, ufvk,
          birthdayHeight: 1, scannedHeight: 10,
          balance: { totalAvailable: this.state.pending || this.state.reserved ? 0 : 10, orchardAvailable: this.state.pending || this.state.reserved ? 0 : 10 } });
        let data: Record<string, unknown> = {};
        switch (message.op) {
          case "init": data = { mode: "multi-thread", threads: 2 }; break;
          case "fromSnapshot": this.state = decode(new Uint8Array(message.snapshot!)); break;
          case "meta": data = { scanned: 10, birthday: 1, nextHeight: 11 }; break;
          case "snapshotJson": data = { json: JSON.stringify(preview()) }; break;
          case "toSnapshot": data = { snapshot: encode(this.state).buffer }; break;
          case "persistenceSnapshot": data = { snapshot: encode(this.state).buffer, json: JSON.stringify(preview()) }; break;
          case "history": data = { json: JSON.stringify(this.state.pending ? [{ txid, status: "pending" }] : []) }; break;
          case "hardware":
            if (message.action === "createSend") this.state.reserved = true;
            if (message.action === stalled) { this.proofId = message.id; this.notifyProof(); return; }
            if (message.action === "prove") this.proofId = message.id;
            if (message.action === "finalize") {
              this.state.pending = true;
              data = { json: JSON.stringify({ hex: "synthetic-hardware-transaction", txid }) }; break;
            }
            data = { bytes: new Uint8Array([1]).buffer }; break;
          case "prove":
            this.state.pending = true; this.proofId = message.id; this.notifyProof(); return;
        }
        queueMicrotask(() => this.reply(message.id, data));
      }
      reply(id: number, data: Record<string, unknown>) { this.dispatchEvent(new MessageEvent("message", { data: { id, ...data } })); }
      terminate() { this.terminated++; }
    }
    const worker = new ProofWorker();
    await attachScanWorker(worker as unknown as Worker, { threads: 2, preferMulticore: true }, () => worker as unknown as Worker);
    let submissions = 0;
    const wallet = await f.open({ unlockPolicy, server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => {
        submissions++;
        if (stalled === "acknowledgement") { worker.notifyProof(); return new Promise<string>(() => {}); }
        return txid;
      } } });
    const created = await wallet.create({ birthday: 1 });
    await wallet.unlock(words);
    const durable = () => f.storage.transaction("readonly", tx => tx.get("default", value => tx.result(value)));
    const saved = await durable();
    const sending = wallet.send("fixture", "0.00005", undefined,
      signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : undefined);
    const rejected = assert.rejects(sending, stalled === "acknowledgement"
      ? error => code("broadcast_failed")(error) && (error as WalletError).txid === txid : code("cancelled"));
    await worker.started;
    if (signing === "hardware" && stalled !== "acknowledgement") {
      // A background checkpoint queued before proving may already contain the
      // hardware reservation. Use real generation/revision CAS to commit it.
      const record = await readSavedSnapshotRecord();
      assert.ok(record);
      worker.state.address = "latest-saved-address";
      await saveWalletSnapshot(encode(worker.state), { network: "regtest", unifiedAddress: worker.state.address,
        ufvk, birthdayHeight: 1, scannedHeight: 10, balance: { totalAvailable: 0, orchardAvailable: 0 } },
        () => true, record.generation, record.key);
    }
    const pendingReceipt = stalled === "acknowledgement" ? await durable() : undefined;
    let closed = false;
    const closing = wallet.close().then(() => { closed = true; });
    try {
      // The worker never replies: teardown must finish from cancellation alone.
      for (let i = 0; i < 10; i++) await new Promise(resolve => setImmediate(resolve));
      assert.equal(closed, true, "close waited for the stalled proof RPC");
      await closing; await rejected;
      assert.equal(worker.terminated, 1);
      if (signing === "software") assert.deepEqual(await durable(), saved, "unsubmitted proof changed the durable wallet");
      if (pendingReceipt) assert.deepEqual(await durable(), pendingReceipt, "uncertain submission lost its durable receipt");
      const next = new ProofWorker();
      await restartScanWorker(() => next as unknown as Worker);
      const reopened = await f.open();
      assert.equal((await reopened.load())?.unifiedAddress, signing === "hardware" && stalled !== "acknowledgement" ? "latest-saved-address" : created.wallet.unifiedAddress);
      assert.equal((await reopened.getWallet()).balance?.totalAvailable, stalled === "acknowledgement" ? 0 : 10);
      assert.deepEqual(await reopened.history(), stalled === "acknowledgement" ? [{ txid, status: "pending" }] : []);
      assert.equal(reopened.hasSpendingSeed(), false);
      worker.reply(worker.proofId!, { hex: "late-synthetic-proof", txid });
      await new Promise(resolve => setImmediate(resolve));
      assert.equal(submissions, stalled === "acknowledgement" ? 1 : 0);
      await wallet.close();
      assert.equal(next.terminated, 0, "retired close terminated the new owner");
    } finally {
      await restartScanWorker();
      await closing.catch(() => {}); await rejected.catch(() => {});
    }
  });
}

test("load hydrates another tab's saved generation and drops the old spending seed", async t => {
  const f=fixture(t), wallet=await f.open({unlockPolicy:"session"});
  const created=await wallet.create({birthday:1});await wallet.unlock(words);
  const before=await readSavedSnapshotRecord();assert.ok(before);
  const generation=await advanceWalletGeneration(undefined,before.generation,true);
  const latest={network:"regtest" as const,unifiedAddress:"other-tab-wallet",ufvk:deriveAccount(words,"regtest").ufvk,
    birthdayHeight:1,scannedHeight:10,balance:{totalAvailable:0,orchardAvailable:0}};
  await saveWalletSnapshot(encode({address:latest.unifiedAddress,pending:true}),latest,()=>true,generation);
  assert.equal((await wallet.load())?.unifiedAddress,latest.unifiedAddress);
  assert.equal((await wallet.getWallet()).unifiedAddress,latest.unifiedAddress);
  assert.equal(wallet.hasSpendingSeed(),false);
  assert.notEqual(created.wallet.unifiedAddress,latest.unifiedAddress);
});
test("load adopts a newer saved revision rather than the in-memory balance", async t => {
  const f=fixture(t),wallet=await f.open();await wallet.create({birthday:1});
  const record=await readSavedSnapshotRecord();assert.ok(record);
  const current=await wallet.getWallet();
  await saveWalletSnapshot(encode({address:current.unifiedAddress,pending:true}),{...current,balance:{...current.balance,totalAvailable:0,orchardAvailable:0}},()=>true,record.generation,record.key);
  assert.equal((await wallet.load())?.balance.totalAvailable,0);
  assert.equal((await wallet.getWallet()).balance.totalAvailable,0);
});


test("load retires an unlocked wallet when its saved record was removed", async t => {
  const f = fixture(t), wallet = await f.open({ unlockPolicy: "session" });
  await wallet.create({ birthday: 1 });
  await wallet.unlock(words);
  await clearSavedSnapshot();
  assert.equal(await wallet.load(), null);
  assert.equal(wallet.hasSpendingSeed(), false);
});

function autoSyncClock(t: TestContext) {
  const callbacks = new Map<number, { callback: () => void; interval: number }>();
  let nextId = 0;
  t.mock.method(globalThis, "setInterval", (callback: () => void, interval: number) => {
    const id = ++nextId;
    callbacks.set(id, { callback, interval });
    return id;
  });
  t.mock.method(globalThis, "clearInterval", (id: number) => callbacks.delete(id));
  const previous = Object.getOwnPropertyDescriptor(globalThis, "document");
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  const document = Object.assign(new EventTarget(), { visibilityState: "visible" });
  Object.defineProperty(globalThis, "document", { configurable: true, value: document });
  Object.defineProperty(globalThis, "window", { configurable: true, value: new EventTarget() });
  t.after(() => {
    previous ? Object.defineProperty(globalThis, "document", previous) : Reflect.deleteProperty(globalThis, "document");
    previousWindow ? Object.defineProperty(globalThis, "window", previousWindow) : Reflect.deleteProperty(globalThis, "window");
  });
  const flush = () => new Promise<void>(resolve => setImmediate(resolve));
  return {
    intervals: () => [...callbacks.values()].map(timer => timer.interval),
    async tick() { for (const timer of callbacks.values()) timer.callback(); await flush(); },
    async visible() { document.dispatchEvent(new Event("visibilitychange")); await flush(); },
    flush,
  };
}

test("auto-sync survives forgetting and follows a new wallet on the same client", async t => {
  const clock = autoSyncClock(t), f = fixture(t);
  let tips = 0;
  const wallet = await f.open({ autoSync: { intervalMs: 3200 }, server: {
    kind: "fixture", label: "offline", tip: async () => { tips++; return 10; },
    blocks: async () => new Uint8Array(), submit: async () => txid,
  } });
  await wallet.create({ birthday: 1 });
  tips = 0;
  await clock.tick(); assert.equal(tips, 1);
  await wallet.forget();
  assert.deepEqual(clock.intervals(), [3200]);
  await clock.tick(); assert.equal(tips, 1, "an empty client must not query the server");
  await wallet.create({ birthday: 1 });
  tips = 1;
  await clock.tick(); assert.equal(tips, 2);
  await clock.visible(); assert.equal(tips, 3, "visibility catch-up was not restored");
  await wallet.close();
  assert.deepEqual(clock.intervals(), []);
  await clock.visible(); assert.equal(tips, 3, "close retained a visibility listener");
});

test("failed forgetting resumes auto-sync for the saved wallet", async t => {
  const clock = autoSyncClock(t), f = fixture(t);
  let available = true, tips = 0;
  const storage = { ...f.storage, get available() { return available; } };
  const wallet = await f.open({ storage, autoSync: true, server: {
    kind: "fixture", label: "offline", tip: async () => { tips++; return 10; },
    blocks: async () => new Uint8Array(), submit: async () => txid,
  } });
  const initial = await wallet.create({ birthday: 1 });
  available = false;
  await assert.rejects(wallet.forget(), code("wallet_db"));
  available = true;
  assert.equal((await wallet.load())?.unifiedAddress, initial.wallet.unifiedAddress);
  tips = 0;
  await clock.tick(); assert.equal(tips, 1);
  await clock.visible(); assert.equal(tips, 2);
});

for (const action of ["stop", "start", "close"] as const) {
  test(`auto-sync honors ${action} while forgetting waits on storage`, { timeout: 5000 }, async t => {
    const clock = autoSyncClock(t), f = fixture(t);
    let hold = false, entered!: () => void, release!: () => void, tips = 0;
    const waiting = new Promise<void>(resolve => { entered = resolve; });
    const released = new Promise<void>(resolve => { release = resolve; });
    const storage: typeof f.storage = {
      available: true,
      async transaction(mode, body, options) {
        if (hold && mode === "readwrite") { hold = false; entered(); await released; }
        return f.storage.transaction(mode, body, options);
      },
    };
    const wallet = await f.open({ storage, autoSync: true, server: {
      kind: "fixture", label: "offline", tip: async () => { tips++; return 10; },
      blocks: async () => new Uint8Array(), submit: async () => txid,
    } });
    await wallet.create({ birthday: 1 });
    tips = 0;
    hold = true;
    const forgetting = wallet.forget();
    await waiting;
    let closing: Promise<void> | undefined;
    try {
      assert.deepEqual(clock.intervals(), []);
      closing = action === "close" ? wallet.close() : undefined;
      if (action === "stop") wallet.stopAutoSync();
      if (action === "start") wallet.startAutoSync(4500);
      await clock.visible(); assert.equal(tips, 0, "forgetting must pause visibility catch-up");
      assert.deepEqual(clock.intervals(), [], "start must not run during deletion");
    } finally {
      release(); await forgetting; await closing;
    }
    assert.deepEqual(clock.intervals(), action === "start" ? [4500] : []);
    if (action !== "close") await wallet.create({ birthday: 1 });
    tips = 0;
    await clock.tick(); await clock.visible();
    assert.equal(tips, action === "start" ? 2 : 0);
  });
}

test("an old auto-sync tip cannot scan a replacement after forgetting", async t => {
  const clock = autoSyncClock(t), f = fixture(t);
  let resolveTip!: (height: number) => void, tips = 0, blocks = 0, delay = false;
  const delayedTip = new Promise<number>(resolve => { resolveTip = resolve; });
  const wallet = await f.open({ autoSync: true, server: {
    kind: "fixture", label: "offline", tip: async () => { tips++; return delay ? delayedTip : 10; },
    blocks: async () => { blocks++; return new Uint8Array(); }, submit: async () => txid,
  } });
  await wallet.create({ birthday: 1 });
  tips = 0; delay = true;
  await clock.tick(); assert.equal(tips, 1);
  await wallet.forget();
  delay = false;
  await wallet.create({ birthday: 1 });
  tips = 1;
  resolveTip(20); await clock.flush();
  assert.equal(blocks, 0, "an old tip result triggered sync on the new wallet");
  await clock.tick(); assert.equal(tips, 2);
});


test("rescan retains wallet identity, persists an earlier birthday and remains locked on reload", async t => {
  const f = fixture(t), wallet = await f.open();
  const created = await wallet.create({ birthday: 8 });
  const rescanned = await wallet.rescan({ birthday: 3 });
  assert.equal(rescanned.unifiedAddress, created.wallet.unifiedAddress);
  assert.equal(rescanned.birthdayHeight, 3);
  assert.equal(rescanned.scannedHeight, 2);
  await wallet.close();
  const reopened = await f.open();
  assert.equal((await reopened.load())?.birthdayHeight, 3);
  assert.equal(reopened.hasSpendingSeed(), false);
});

for (const memoFetch of ["shared", "auto", "on-demand"] as const) {
  test(`rescan resets public-data progress only after its durable commit (${memoFetch})`, async t => {
    const f = fixture(t);
    let failWrites = false, resets = 0;
    const storage: typeof f.storage = { available: true,
      transaction(mode, body, options) {
        if (failWrites && mode === "readwrite") return Promise.reject(new DOMException("quota", "QuotaExceededError"));
        return f.storage.transaction(mode, body, options);
      } };
    const transparentScan = memoFetch === "shared" ? "compact" : "off";
    const wallet = await f.open({ storage, memoFetch, transparentScan, server: {
      kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
      info: async () => ({ chain: "regtest", protocolVersion: "v0.5.0", transparentCompact: true }),
      transparentBlocks: async (_start, end) => {
        const bytes = new Uint8Array(4); new DataView(bytes.buffer).setUint32(0, end); return bytes;
      },
      sharedMemos: async (start, end) => JSON.stringify({ start, end }),
      tx: async () => { throw new Error("an empty memo queue must not fetch transactions"); },
    } });
    await wallet.create({ birthday: 8 });
    await wallet.sync();
    await wallet.fetchMemos();
    const before = await wallet.getWallet();
    assert.equal(before.transparentScanStatus, transparentScan === "compact" ? "complete" : "off");
    assert.equal(before.sharedMemoStatus, memoFetch === "shared" ? "complete" : "off");
    assert.equal(before.memoFetchStatus, "complete");
    const saved = await readSavedSnapshotRecord();
    // Fail after the engine has cleared coverage, not during the preceding save.
    f.onRescan(() => { resets++; failWrites = true; });
    await assert.rejects(wallet.rescan({ birthday: 3 }), code("storage_full"));
    assert.equal(resets, 1);
    assert.deepEqual(await wallet.getWallet(), before);
    assert.deepEqual(await readSavedSnapshotRecord(), saved);
    failWrites = false; f.onRescan();
    const after = await wallet.rescan({ birthday: 3 });
    assert.equal(after.birthdayHeight, 3);
    assert.equal(after.scannedHeight, 2);
    assert.equal(after.transparentScanHeight, null);
    assert.equal(after.memoScanHeight, null);
    assert.equal(after.transparentScanStatus, transparentScan === "compact" ? "scanning" : "off");
    assert.equal(after.sharedMemoStatus, memoFetch === "shared" ? "scanning" : "off");
    assert.equal(after.memoFetchStatus, memoFetch === "on-demand" ? "off" : "scanning");
  });
}

test("rescan rejects invalid/later birthdays and unknown outgoing payments without changing saved state", async t => {
  const f = fixture(t), wallet = await f.open();
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  await assert.rejects(wallet.rescan({ birthday: "" }), code("invalid_birthday"));
  await assert.rejects(wallet.rescan({ birthday: "auto" }), code("invalid_birthday"));
  await assert.rejects(wallet.rescan({ birthday: "yesterday" }), code("invalid_birthday"));
  await assert.rejects(wallet.rescan({ birthday: 9 }), code("rescan_later_birthday"));
  assert.deepEqual(await readSavedSnapshotRecord(), before);
  await wallet.unlock(words);
  await wallet.send("fixture", "0.00005");
  const pending = await readSavedSnapshotRecord();
  await assert.rejects(wallet.rescan({ birthday: 3 }), code("rescan_pending"));
  assert.deepEqual(await readSavedSnapshotRecord(), pending);
  assert.deepEqual(await wallet.history(), [{ txid, status: "pending" }]);
});

test("rescan rejects a birthday above a lagging server tip without clearing committed activity", async t => {
  const f = fixture(t);
  let tip = 10;
  const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => tip,
    blocks: async () => new Uint8Array(), submit: async () => txid } });
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  tip = 4;
  await assert.rejects(wallet.rescan({ birthday: 6 }), code("birthday_above_tip"));
  assert.deepEqual(await readSavedSnapshotRecord(), before);
  assert.equal((await wallet.load())?.birthdayHeight, 8);
  assert.equal((await wallet.load())?.scannedHeight, 10);
});

test("rescan rolls back in-memory state when a mandatory storage commit fails", async t => {
  const f = fixture(t);
  let fail = false;
  const storage: typeof f.storage = { available: true,
    transaction(mode, body, options) {
      if (fail && mode === "readwrite") return Promise.reject(new DOMException("quota", "QuotaExceededError"));
      return f.storage.transaction(mode, body, options);
    } };
  const wallet = await f.open({ storage });
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  fail = true;
  await assert.rejects(wallet.rescan({ birthday: 3 }), code("storage_full"));
  assert.equal((await wallet.getWallet()).birthdayHeight, 8);
  assert.deepEqual(await readSavedSnapshotRecord(), before);
  fail = false;
  assert.equal((await wallet.rescan({ birthday: 3 })).birthdayHeight, 3);
});

test("a stale rescan adopts the winning tab's pending state instead of rolling it back", async t => {
  const f = fixture(t);
  let held = false, arrived!: () => void, release!: () => void;
  const waiting = new Promise<void>(resolve => { arrived = resolve; });
  const gate = new Promise<void>(resolve => { release = resolve; });
  const storage: typeof f.storage = { available: true,
    async transaction(mode, body, options) {
      if (held && mode === "readwrite") { held = false; arrived(); await gate; }
      return f.storage.transaction(mode, body, options);
    } };
  const wallet = await f.open({ storage });
  await wallet.create({ birthday: 8 });
  const record = (await readSavedSnapshotRecord())!;
  held = true;
  const rescan = wallet.rescan({ birthday: 3 });
  await waiting;
  const other = { ...decode(record.bytes), address: "winning-tab-address", pending: true, birthday: 7 };
  await saveWalletSnapshot(encode(other), { network: "regtest", unifiedAddress: other.address,
    birthdayHeight: 7, scannedHeight: 10, balance: { totalAvailable: 0, orchardAvailable: 0 } },
    () => true, record.generation, record.key);
  release();
  await assert.rejects(rescan, code("wallet_changed"));
  assert.equal((await wallet.getWallet()).unifiedAddress, other.address);
  assert.equal((await wallet.getWallet()).birthdayHeight, 7);
  assert.deepEqual(await wallet.history(), [{ txid, status: "pending" }]);
});

test("close during rescan leaves the original committed birthday available on reopening", async t => {
  const f = fixture(t);
  let held = false, arrived!: () => void, release!: () => void;
  const waiting = new Promise<void>(resolve => { arrived = resolve; });
  const gate = new Promise<void>(resolve => { release = resolve; });
  const storage: typeof f.storage = { available: true,
    async transaction(mode, body, options) {
      if (held && mode === "readwrite") { held = false; arrived(); await gate; }
      return f.storage.transaction(mode, body, options);
    } };
  const wallet = await f.open({ storage });
  await wallet.create({ birthday: 8 });
  const original = await readSavedSnapshotRecord();
  held = true;
  const rescan = wallet.rescan({ birthday: 3 });
  await waiting;
  await wallet.close();
  release();
  await assert.rejects(rescan, code("cancelled"));
  const reopened = await f.open();
  assert.deepEqual(await readSavedSnapshotRecord(), original);
  assert.equal((await reopened.load())?.birthdayHeight, 8);
});


test("every sync entry point refuses to mutate a rescan waiting for its durable save", async t => {
  const f = fixture(t);
  let held = false, arrived!: () => void, release!: () => void;
  const waiting = new Promise<void>(resolve => { arrived = resolve; });
  const gate = new Promise<void>(resolve => { release = resolve; });
  const storage: typeof f.storage = { available: true,
    async transaction(mode, body, options) {
      if (held && mode === "readwrite") { held = false; arrived(); await gate; }
      return f.storage.transaction(mode, body, options);
    } };
  const wallet = await f.open({ storage });
  await wallet.create({ birthday: 8 });
  held = true;
  const rescan = wallet.rescan({ birthday: 3 });
  await waiting;
  await assert.rejects(wallet.sync(), code("busy"));
  await assert.rejects(wallet.waitUntilCaughtUp({ timeoutMs: 100 }), code("busy"));
  await assert.rejects(wallet.fetchMemos(), code("busy"));
  await assert.rejects(wallet.shield(), code("busy"));
  release();
  assert.equal((await rescan).birthdayHeight, 3);
});

test("a deep rescan needs client opt-in and rejects before changing state", async t => {
  const f = fixture(t), wallet = await f.open({ server: {
    kind: "fixture", label: "offline", tip: async () => 200000, blocks: async () => new Uint8Array(),
  } });
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  await assert.rejects(wallet.rescan({ birthday: 3 }), code("deep_sync_rejected"));
  assert.deepEqual(await readSavedSnapshotRecord(), before);
  assert.equal((await wallet.getWallet()).birthdayHeight, 8);
});


test("a failed in-memory rescan rollback retires the source and keeps the original save error", async t => {
  const f = fixture(t);
  let fail = false;
  const storage: typeof f.storage = { available: true,
    transaction(mode, body, options) {
      if (fail && mode === "readwrite") return Promise.reject(new DOMException("quota", "QuotaExceededError"));
      return f.storage.transaction(mode, body, options);
    } };
  const wallet = await f.open({ storage });
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  f.onRevive(() => { throw new Error("injected rollback failure"); });
  fail = true;
  await assert.rejects(wallet.rescan({ birthday: 3 }), code("storage_full"));
  assert.ok(f.frees() > 0, "unsaved scan session must be retired");
  f.onRevive(); fail = false;
  assert.equal((await wallet.getWallet()).birthdayHeight, 8);
  assert.deepEqual(await readSavedSnapshotRecord(), before);
});


test("numeric birthday creation is offline; automatic creation reports a stable transport failure", async t => {
  const f = fixture(t);
  let tips = 0;
  const wallet = await f.open({server: {
    kind: "fixture", label: "unavailable", tip: async () => { tips++; throw new Error("SYNTHETIC_PRIVATE_PROVIDER_CONTEXT"); },
    blocks: async () => { throw new Error("Unexpected sync"); },
  }});
  await assert.rejects(wallet.create({birthday: 0}), code("invalid_birthday"));
  assert.equal(tips, 0);
  const created = await wallet.create({birthday: 1});
  assert.ok(created.wallet.unifiedAddress);
  assert.equal(tips, 0, "offline creation must not contact the light server");
  await wallet.forget();
  await assert.rejects(wallet.create({birthday: "auto"}), error => {
    assert.ok(error instanceof WalletError);
    assert.equal(error.code, "transport");
    assert.equal(error.userMessage(), "Could not reach the light server.");
    assert.doesNotMatch(error.userMessage(), /SYNTHETIC_PRIVATE_PROVIDER_CONTEXT/);
    return true;
  });
  assert.equal(tips, 1);
  assert.equal(await wallet.load(), null, "failed automatic creation saved a wallet");
});


test("public unlock reports malformed words without granting spending access", async t => {
  const f = fixture(t);
  const wallet = await f.open();
  await wallet.create({ birthday: 1 });
  wallet.lock();
  await assert.rejects(wallet.unlock("not a phrase"), error => {
    assert.ok(error instanceof WalletError);
    assert.equal(error.code, "invalid_recovery_phrase");
    assert.equal(error.userMessage(), "Those words are not a valid recovery phrase.");
    assert.doesNotMatch(error.userMessage(), /SYNTHETIC_PRIVATE_WORD/);
    return true;
  });
  assert.equal(wallet.hasSpendingSeed(), false);
  await wallet.unlock(words);
  assert.equal(wallet.hasSpendingSeed(), true);
});


test("pending-aware forget adopts another tab's reservation before checking deletion", async t => {
  const f = fixture(t);
  const wallet = await f.open(); await wallet.create({ birthday: 1 });
  const baseline = await readSavedSnapshotRecord(); assert.ok(baseline);
  assert.equal("forgetSavedWallet" in wallet, false, "internal deletion helper must not leak into the root facade");
  await assert.rejects(wallet.forget({ pending: "typo" } as never), code("unknown"));
  assert.deepEqual(await readSavedSnapshotRecord(), baseline, "invalid policy must not silently perform explicit deletion");
  // Another realm commits a pending send. This client's history is deliberately stale.
  const state = decode(baseline.bytes); state.pending = true;
  await saveWalletSnapshot(encode(state), await wallet.getWallet(), () => true, baseline.generation);
  const committed = await readSavedSnapshotRecord();
  assert.equal((await wallet.pending()).length, 0);
  await assert.rejects(wallet.forget({ passkey: true, pending: "reject" }), code("forget_pending"));
  assert.deepEqual(await readSavedSnapshotRecord(), committed);
  assert.equal((await wallet.pending()).length, 1, "guard must adopt the durable pending send");
  // Explicit deletion remains application-controlled when the pending policy is omitted.
  await wallet.forget({ passkey: true });
  assert.equal(await readSavedSnapshotRecord(), null);
});
