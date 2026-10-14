import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test, before, type TestContext } from "node:test";
import { createWallet, forgetWallet, type Wallet } from "../src/create-wallet.ts";
import { memoryWalletStorage } from "../src/storage.ts";
import { createWasmClient, attachWasmBindings } from "../src/wasm-client.ts";
import { initialize, deriveAccount } from "../src/runtime.ts";
import { WalletError } from "@z-stack/core";
import { keystoneSigner } from "../src/hardware.ts";
import { saveWalletSnapshot, readSavedSnapshotRecord, advanceWalletGeneration, clearSavedSnapshot } from "../src/snapshot-storage.ts";
import { attachScanWorker, restartScanWorker } from "../src/scan-host.ts";
import { checkWalletSetup } from "../src/diagnostics.ts";
import { BroadcastRejection, assertCompactBlockHeight, SDK_TRANSPORT_CAPABILITIES } from "../src/lwd.ts";

// Public BIP-39 fixture; no network, real money, environment or private secrets.
const words = [...Array(23).fill("abandon"), "art"].join(" ");
const txid = "ab".repeat(32);
const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const decode = (bytes: Uint8Array) => JSON.parse(new TextDecoder().decode(bytes));
const probeBlock = (height: number) => new Uint8Array([0, 0, 0, 36, 16, height, 26, 32, ...new Array(32).fill(0)]);
before(async () => initialize({ wasmModule: readFileSync(new URL("../src/generated/z_wasm_bg.wasm", import.meta.url)),
  prewarmProveWorker: false, prewarmProvingKey: false }));

function fixture(t: TestContext) {
  const storage = memoryWalletStorage();
  const ufvk = deriveAccount(words, "regtest").ufvk;
  let serial = 0, frees = 0;
  let onSnapshot: (() => void) | undefined;
  let onRevive: (() => void) | undefined;
  let onRescan: (() => void) | undefined;
  let onPrivateTransparent: ((operation: string, input: Uint8Array) => Uint8Array | Promise<Uint8Array>) | undefined;
  const make = (initial: { address: string; pending: boolean; reserved?: boolean; birthday?: number; scanned?: number; transparentScanHeight?: number | null; memoScanHeight?: number | null }) => {
    const state = { ...initial };
    return {
      free() { frees++; },
      privateTransparent: (operation: string, input: Uint8Array) => {
        if (!onPrivateTransparent) throw new Error("unexpected_private_scan");
        return onPrivateTransparent(operation, input);
      },
      toSnapshot: () => { onSnapshot?.(); return encode(state); },
      snapshotJson: () => JSON.stringify({ network: "regtest", unifiedAddress: state.address, ufvk,
        birthdayHeight: state.birthday ?? 8, scannedHeight: state.scanned ?? 10,
        transparentScanHeight: state.transparentScanHeight ?? null, memoScanHeight: state.memoScanHeight ?? null,
        balance: { totalAvailable: state.pending || state.reserved ? 0 : 10, orchardAvailable: state.pending || state.reserved ? 0 : 10, transparentAvailable: 0 } }),
      scannedHeight: () => state.scanned ?? 10,
      nextHeight: () => (state.scanned ?? 10) + 1,
      rewindTo(height: number) { state.scanned = height; return height; },
      rewindToChecked(height: number) { if ((state.scanned ?? 10) - height > 2048) throw new Error("reorg checkpoint is outside retained history"); state.scanned = height; return height; },
      applyCompactBlocksSummary(bytes: Uint8Array) {
        state.scanned = bytes[5];
        return JSON.stringify({ scanned: state.scanned, notesFound: 0, spendsFound: 0 });
      },
      recomputePools() {},
      birthday: () => state.birthday ?? 8,
      treesReady: () => false,
      pendingRawTxs: () => JSON.stringify(state.pending ? ["synthetic-transaction"] : []),
      pendingSubmissions: () => JSON.stringify(state.pending ? [{ txid, hex: "synthetic-transaction" }] : []),
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
      historyQuery: (_limit: number, status: string, id: string) => JSON.stringify(state.pending && (!status || status === "pending") && (!id || id === txid) ? [{ txid, status: "pending" }] : []),
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
    onPrivateTransparent(callback: NonNullable<typeof onPrivateTransparent>) { onPrivateTransparent = callback; },
    async open(extra: Partial<Parameters<typeof createWallet>[0]> = {}) {
      wallet = await createWallet({ network: "regtest", storage, prewarmProvingKey: false,
        server: { kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(), submit: async () => txid }, ...extra });
      return wallet;
    },
  };
}
const code = (expected: string) => (error: unknown) => error instanceof WalletError && error.code === expected;

for (const tip of [9, 10]) {
  test(`explicit chain verification checks a ${tip === 9 ? "shorter" : "unchanged"} tip and preserves pending payment`, async t => {
    const f = fixture(t);
    const ranges: number[][] = [];
    let currentTip = 10;
    const wallet = await f.open({ verifyChainTip: true, server: {
      kind: "fixture", label: "offline", tip: async () => currentTip,
      blocks: async (start, end) => { ranges.push([start, end]); return probeBlock(end); },
      submit: async () => txid,
    } });
    await wallet.create({ birthday: 8 });
    await wallet.unlock(words);
    await wallet.send("fixture", "0.00005");
    currentTip = tip;
    const result = await wallet.sync();
    assert.deepEqual(ranges, [[tip, tip]]);
    assert.equal(result.scannedHeight, tip);
    assert.equal((await wallet.history())[0]?.txid, txid);
    await wallet.close();
    const reopened = await f.open();
    assert.equal((await reopened.load())?.scannedHeight, tip);
    assert.equal((await reopened.history())[0]?.txid, txid);
  });
}

test("chain verification fails closed on unavailable overlap blocks", async t => {
  const f = fixture(t);
  const wallet = await f.open({ verifyChainTip: true });
  await wallet.create({ birthday: 8 });
  await assert.rejects(wallet.sync());
});

test("strict missing frontier refuses before overlap retrieval or wallet rewind", async t => {
  const f = fixture(t);
  const ranges: number[][] = [];
  const wallet = await f.open({ verifyChainTip: true, requireBirthdayTreeState: true, server: {
    kind: "fixture", label: "offline", tip: async () => 9,
    blocks: async (start, end) => { ranges.push([start, end]); return probeBlock(end); },
  } });
  await wallet.create({ birthday: 8 });
  const before = await wallet.getWallet();
  await assert.rejects(wallet.sync(), /requires a birthday tree state/);
  assert.deepEqual(ranges, []);
  assert.equal((await wallet.getWallet()).scannedHeight, before.scannedHeight);
});

test("chain probes reject wrong heights, missing hashes and extra blocks", () => {
  const valid = probeBlock(10);
  assert.doesNotThrow(() => assertCompactBlockHeight(valid, 10));
  assert.throws(() => assertCompactBlockHeight(valid, 9));
  assert.throws(() => assertCompactBlockHeight(new Uint8Array([...valid, ...valid]), 10));
  assert.throws(() => assertCompactBlockHeight(new Uint8Array([0, 0, 0, 2, 16, 10]), 10));
});

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
    blocks: async () => new Uint8Array(), submit: async (_hex, _signal, context) => { assert.deepEqual(context, { txid }); submissions++; return txid; } } });
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
  await assert.rejects(createWallet({ network: "regtest", server: "https://unused.invalid" }), code("owner_conflict"));
  await assert.rejects(forgetWallet({ storage: f.storage }), code("owner_conflict"));
  assert.equal((await wallet.getWallet()).unifiedAddress, initial.wallet.unifiedAddress);
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

test("software spend preserves an app-owned WalletError from a late broadcast guard and rolls back", async t => {
  const f = fixture(t);
  let expired = false, submissions = 0;
  const wallet = await f.open({server:{kind:"fixture",label:"offline",tip:async()=>10,
    blocks:async()=>new Uint8Array(),submit:async()=>{submissions++;return txid;}}});
  await wallet.create({birthday:1}); await wallet.unlock(words);
  class ReviewOutdated extends WalletError {
    override readonly name = "ReviewOutdatedError";
    constructor() { super("cancelled", "This payment review is out of date."); }
  }
  const error = new ReviewOutdated();
  f.onRevive(()=>{expired=true;});
  await assert.rejects(wallet.send("fixture", "0.00005", undefined, {
    beforeBroadcast:()=>{if(expired) throw error;return true;},
  }), caught=>caught===error);
  assert.equal(expired,true,"the guard expires during transaction construction");
  assert.equal(submissions,0);
  assert.deepEqual(await wallet.pending(),[]);
  assert.deepEqual(await wallet.history(),[]);
  assert.equal((await wallet.getWallet()).balance.totalAvailable,10);
  assert.equal(wallet.hasSpendingSeed(),false);
});

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
          case "historyQuery":
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

test("public rescan normalizes an active engine reservation without changing the saved wallet", async t => {
  const f = fixture(t), wallet = await f.open();
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  f.onRescan(() => { throw "rescan active output reservation"; });
  await assert.rejects(wallet.rescan({ birthday: 3 }), code("rescan_pending"));
  assert.deepEqual(await readSavedSnapshotRecord(), before);
  assert.equal((await wallet.getWallet()).birthdayHeight, 8);
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

test("restore rejects a birthday the client would refuse to sync before saving", async t => {
  const f = fixture(t), wallet = await f.open({ server: {
    kind: "fixture", label: "offline", tip: async () => 200000, blocks: async () => new Uint8Array(),
  } });
  await assert.rejects(wallet.restore(words), code("deep_sync_rejected"));
  await assert.rejects(wallet.restore(words, { birthday: 3 }), code("deep_sync_rejected"));
  assert.equal(await wallet.load(), null, "an unsyncable restore was saved");
  await wallet.restore(words, { birthday: 100000 });
  assert.ok(await wallet.load(), "a syncable restore was not saved");
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
  await assert.rejects(wallet.restore(words, { birthday: 1 }), error => {
    assert.ok(error instanceof WalletError);
    assert.equal(error.code, "transport");
    assert.doesNotMatch(error.userMessage(), /SYNTHETIC_PRIVATE_PROVIDER_CONTEXT/);
    return true;
  });
  assert.equal(await wallet.load(), null, "failed restore saved a wallet");
});


test("creation and phrase restoration preserve permanent light-server error codes", async t => {
  const f = fixture(t);
  let error: unknown = new Error("HTTP 401: bridge token required");
  const wallet = await f.open({ server: {
    kind: "fixture", label: "protected", tip: async () => { throw error; },
    blocks: async () => new Uint8Array(),
  } });
  for (const failure of [
    { error, expected: "auth" },
    { error: new WalletError("chain_mismatch", "wrong chain"), expected: "chain_mismatch" },
  ]) {
    error = failure.error;
    await assert.rejects(wallet.create({ birthday: "auto" }), code(failure.expected));
    await assert.rejects(wallet.restore(words, { birthday: 1 }), code(failure.expected));
    assert.equal(await wallet.load(), null);
  }
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

for (const status of ["scanning", "unavailable"] as const) {
  test(`auto-sync ignores manual memo ${status} at an unchanged tip and follows policy changes`, async t => {
    const clock = autoSyncClock(t), f = fixture(t); let tip = 10;
    const wallet = await f.open({autoSync:true, server:{kind:"fixture",label:"offline",tip:async()=>tip,blocks:async()=>new Uint8Array()}});
    await wallet.create({birthday:1});
    const snapshot = {...await wallet.getWallet(),transparentScanStatus:"off" as const,memoFetchStatus:status};
    t.mock.method(wallet,"getWallet",async()=>snapshot);
    let syncs = 0; t.mock.method(wallet,"sync",async()=>{syncs++; return snapshot;});
    await clock.tick(); assert.equal(syncs,0,"default on-demand must not keep resyncing a manual queue");
    wallet.setMemoFetch("auto"); await clock.tick(); assert.equal(syncs,1);
    wallet.setMemoFetch("on-demand"); await clock.tick(); assert.equal(syncs,1);
    wallet.setMemoFetch("shared"); await clock.tick(); assert.equal(syncs,2);
    wallet.setMemoFetch("on-demand"); tip = 11;
    await clock.tick(); assert.equal(syncs,3,"new blocks still trigger background sync");
    tip = 10; Object.assign(snapshot,{transparentScanStatus:"scanning"});
    await clock.tick(); assert.equal(syncs,4,"transparent automatic work is independent of the manual memo policy");
  });
}


test("automatic restore birthdays reject before fetching or modifying saved state", async t => {
  const f = fixture(t);
  let tips = 0;
  const wallet = await f.open({ deepSync: true, server: {
    kind: "fixture", label: "offline", tip: async () => { tips++; return 200000; }, blocks: async () => new Uint8Array(),
  } });
  for (const birthday of ["auto", " AUTO ", "", "  "]) {
    await assert.rejects(wallet.restore(words, { birthday }), code("invalid_birthday"));
  }
  assert.equal(tips, 0);
  assert.equal(await wallet.load(), null);
});

test("configured zero outage grace stops after the first failed tip and keeps saved state", async t => {
  const f = fixture(t);
  let tips = 0;
  const wallet = await f.open({ lightServerGraceMs: 0, server: {
    kind: "fixture", label: "offline", tip: async () => { tips++; throw new TypeError("SYNTHETIC_PRIVATE_PROVIDER_DETAIL"); },
    blocks: async () => new Uint8Array(),
  } });
  const created = await wallet.create({ birthday: 1 });
  const before = await readSavedSnapshotRecord();
  await assert.rejects(wallet.sync(), code("transport"));
  assert.equal(tips, 1);
  assert.deepEqual(await readSavedSnapshotRecord(), before);
  assert.equal((await wallet.getWallet()).unifiedAddress, created.wallet.unifiedAddress);
});

test("invalid outage configuration releases the owner for a later client", async t => {
  for (const lightServerGraceMs of [-1, Infinity, 0.5, 2_147_483_648]) {
    await assert.rejects(createWallet({ network: "regtest", storage: memoryWalletStorage(), server: "https://unused.invalid", lightServerGraceMs }), code("unknown"));
  }
  const f = fixture(t);
  assert.ok(await f.open());
});

for (const signing of ["software", "hardware"] as const) {
  test(`${signing} beforeSubmit waits for the application journal after durable reservation`, async t => {
    const f = fixture(t);
    let submissions = 0, release!: () => void, entered!: () => void;
    const journal = new Promise<void>(resolve => { release = resolve; });
    const waiting = new Promise<void>(resolve => { entered = resolve; });
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async (_hex, _signal, context) => { assert.deepEqual(context, { txid }); submissions++; return txid; } } });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    const sending = wallet.send("fixture", "0.00005", undefined, {
      ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
      beforeSubmit: async (receipt, signal) => {
        assert.equal(receipt.txid, txid);
        signal.throwIfAborted();
        const saved = await readSavedSnapshotRecord();
        assert.ok(saved && decode(saved.bytes).pending, "reservation must be committed before the hook");
        entered(); await journal;
      },
    });
    await waiting;
    assert.equal(submissions, 0, "network submission cannot race the journal");
    release(); await sending;
    assert.equal(submissions, 1);
  });
  test(`${signing} failed beforeSubmit rolls back without network submission`, async t => {
    const f = fixture(t);
    let submissions = 0;
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005", undefined, {
      ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
      beforeSubmit: async () => { throw new WalletError("storage_full", "journal full"); },
    }), code("storage_full"));
    assert.equal(submissions, 0);
    assert.deepEqual(await wallet.pending(), []);
    assert.equal(wallet.hasSpendingSeed(), false);
  });
}

for (const signing of ["software", "hardware"] as const) {
  for (const cancel of ["review", "signal", "close"] as const) {
    test(`${signing} retains signed bytes when ${cancel} races a successful journal commit`, async t => {
      const f = fixture(t);
      let submissions = 0, release!: () => void, entered!: () => void, allowed = true;
      const abort = new AbortController();
      const journal = new Promise<void>(resolve => { release = resolve; });
      const waiting = new Promise<void>(resolve => { entered = resolve; });
      const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
        blocks: async () => new Uint8Array(), submit: async () => { submissions++; return txid; } } });
      await wallet.create({ birthday: 1 });
      if (signing === "software") await wallet.unlock(words);
      const sending = wallet.send("fixture", "0.00005", undefined, {
        ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
        signal: abort.signal, beforeBroadcast: () => allowed,
        beforeSubmit: async () => { entered(); await journal; },
      });
      // Attach rejection handling before triggering cancellation or Close.
      const result = assert.rejects(sending, (error: unknown) => error instanceof WalletError &&
        error.code === "broadcast_failed" && error.txid === txid);
      await waiting;
      let closing: Promise<void> | undefined;
      if (cancel === "review") allowed = false;
      else if (cancel === "signal") abort.abort();
      else closing = wallet.close();
      release();
      await result;
      await closing;
      assert.equal(submissions, 0);
      if (closing) {
        const reopened = await f.open();
        assert.equal((await reopened.load())?.balance?.totalAvailable, 0);
        assert.deepEqual((await reopened.pending()).map(entry => entry.txid), [txid]);
      } else {
        const saved = await readSavedSnapshotRecord();
        assert.ok(saved && decode(saved.bytes).pending, "committed receipt must retain its signed transaction");
      }
    });
  }
}

test("chain verification refuses an older tip outside the retained horizon without changing the wallet", async t => {
  const f = fixture(t);
  let tip = 5000;
  const wallet = await f.open({ verifyChainTip: true, server: { kind: "fixture", label: "offline",
    tip: async () => tip, blocks: async (start: number) => probeBlock(start) } });
  await wallet.create({ birthday: 1 });
  // Persist a previously scanned wallet beyond the retained-hash window.
  const saved = await readSavedSnapshotRecord();
  assert.ok(saved);
  await saveWalletSnapshot(encode({ ...decode(saved.bytes), scanned: 5000 }), { ...await wallet.getWallet(), scannedHeight: 5000 }, () => true, saved.generation, saved.key);
  await wallet.close();
  tip = 100;
  const reopened = await f.open({ verifyChainTip: true, server: { kind: "fixture", label: "offline",
    tip: async () => tip, blocks: async (start: number) => probeBlock(start) } });
  assert.equal((await reopened.load())?.scannedHeight, 5000);
  await assert.rejects(reopened.sync());
  assert.equal((await reopened.getWallet()).scannedHeight, 5000);
});

for (const signing of ["software", "hardware"] as const) {
  test(`${signing} definitive rejection retains receipt identity and releases only its reservation`, async t => {
    const f = fixture(t);
    let savedId: string | undefined;
    const wallet = await f.open({ server: { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { throw new BroadcastRejection(16, "invalid transaction"); } } });
    await wallet.create({ birthday: 1 });
    if (signing === "software") await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005", undefined, {
      ...(signing === "hardware" ? { signer: keystoneSigner(async bytes => bytes) } : {}),
      beforeSubmit: async receipt => { savedId = receipt.txid; },
    }), (error: unknown) => error instanceof WalletError && error.code === "broadcast_rejected" && error.txid === savedId);
    assert.equal(savedId, txid);
    assert.deepEqual(await wallet.pending(), []);
    assert.equal((await wallet.getWallet()).balance.totalAvailable, 10);
  });
}


test("compact request capability is explicit and frozen", () => {
  assert.equal(Reflect.get(SDK_TRANSPORT_CAPABILITIES, "maxBlocksPerRequest"), true);
  assert.equal(Object.isFrozen(SDK_TRANSPORT_CAPABILITIES), true);
});

test("compact request cap bounds actual prefetch and cannot grow after creation", async t => {
  const f = fixture(t), ranges: number[][] = [];
  const server = { kind: "fixture", label: "offline", maxBlocksPerRequest: 3,
    tip: async () => 22,
    blocks: async (start: number, end: number) => { ranges.push([start, end]); return probeBlock(end); } };
  const wallet = await f.open({ server });
  await wallet.create({ birthday: 8 });
  server.maxBlocksPerRequest = 4000;
  const result = await wallet.sync();
  assert.deepEqual(ranges, [[11, 13], [14, 16], [17, 19], [20, 22]]);
  assert.equal(result.scannedHeight, 22);
});

for (const limit of [undefined, 1, 1000, 4000]) {
  test(`compact first request preserves tuned defaults or cap (${limit ?? "absent"})`, async t => {
    const f = fixture(t), ranges: number[][] = [];
    const server = { kind: "fixture", label: "offline", maxBlocksPerRequest: limit,
      tip: async () => 4010, blocks: async (start: number, end: number) => {
        ranges.push([start, end]); throw new Error("fixture stops before apply");
      } };
    const wallet = await f.open({ server, lightServerGraceMs: 0 });
    await wallet.create({ birthday: 8 });
    await assert.rejects(wallet.sync(), /fixture stops before apply/);
    assert.deepEqual(ranges[0], [11, 10 + (limit ?? 4000)]);
    assert.ok(ranges.every(([start, end]) => end - start + 1 <= (limit ?? 4000)));
  });
}

test("unsafe compact caps refuse before owner claim, observers or provider I/O", async t => {
  const f = fixture(t), existing = await f.open();
  let calls = 0;
  for (const value of [0, -1, 1.5, NaN, Infinity, -Infinity, 4001, Number.MAX_SAFE_INTEGER, "1000", null]) {
    const server = { kind: "fixture", label: "offline", tip: async () => { calls++; return 10; },
      blocks: async () => { calls++; return new Uint8Array(); } };
    Object.defineProperty(server, "maxBlocksPerRequest", { value });
    await assert.rejects(createWallet({ network: "regtest", server, storage: f.storage,
      onLoadProgress: () => { calls++; } }), code("unknown"));
    assert.throws(() => createWasmClient({ network: "regtest", transport: server }), code("unknown"));
  }
  assert.equal(calls, 0);
  await existing.create({ birthday: 8 });
  assert.equal((await existing.getWallet()).scannedHeight, 10);
});


test("compact cap never enlarges public transport tuning", async t => {
  const f = fixture(t), ranges: number[][] = [];
  const wallet = await f.open({ lightServerGraceMs: 0, server: {
    kind: "fixture", label: "https://zec.rocks", maxBlocksPerRequest: 4000,
    tip: async () => 2010, blocks: async (start, end) => {
      ranges.push([start, end]); throw new Error("fixture stops before apply");
    },
  } });
  await wallet.create({ birthday: 8 });
  await assert.rejects(wallet.sync(), /fixture stops before apply/);
  assert.deepEqual(ranges[0], [11, 1010]);
  assert.ok(ranges.every(([start, end]) => end - start + 1 <= 1000));
});

for (const policy of [undefined, "saved-bytes-auto", "explicit"] as const) {
  test(`${policy ?? "default"} submission policy controls sync of a durable unknown payment after reopen`, async t => {
    const f = fixture(t), submitted: string[] = [];
    const server = { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async (hex: string, _signal?: AbortSignal, context?: { readonly txid: string }) => { assert.deepEqual(context, { txid }); submitted.push(hex); throw new TypeError("fixture lost acknowledgement"); } };
    const options = { server, submissionPolicy: policy };
    const wallet = await f.open(options);
    await wallet.create({ birthday: 8 });
    await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
    assert.deepEqual(submitted, ["synthetic-transaction"]);
    assert.equal((await wallet.pending())[0]?.txid, txid);
    submitted.length = 0;
    await wallet.sync();
    assert.deepEqual(submitted, policy === "explicit" ? [] : ["synthetic-transaction"]);
    await wallet.close();
    const reopened = await f.open(options);
    await reopened.load();
    submitted.length = 0;
    await reopened.sync();
    assert.deepEqual(submitted, policy === "explicit" ? [] : ["synthetic-transaction"]);
    assert.equal((await reopened.pending())[0]?.txid, txid);
    assert.equal((await reopened.getWallet()).balance.totalAvailable, 0);
  });
}

test("shared memo transport hint is captured before ownership and keeps common range boundaries", async t => {
  const f = fixture(t), ranges: number[][] = [];
  let advertised = 1;
  const server = { kind: "fixture", label: "offline", get maxSharedMemoBlocksPerRequest() {
    const first = advertised; advertised = 10; return first;
  }, tip: async () => 10, blocks: async () => new Uint8Array(), sharedMemos: async (start: number, end: number) => {
    ranges.push([start, end]); return JSON.stringify({ start, end });
  } };
  const wallet = await f.open({ server, memoFetch: "shared" });
  await wallet.create({ birthday: 8 });
  const snapshot = await wallet.sync();
  assert.deepEqual(ranges, [[8, 8], [9, 9], [10, 10]]);
  assert.equal(snapshot.memoScanHeight, 10);
  await wallet.sync();
  assert.deepEqual(ranges, [[8, 8], [9, 9], [10, 10]], "complete coverage must not fetch again");
});

test("unsafe memo hints and submission policies refuse before ownership, storage or provider I/O", async t => {
  const f = fixture(t), existing = await f.open();
  let calls = 0;
  const server = { kind: "fixture", label: "offline", tip: async () => { calls++; return 10; }, blocks: async () => { calls++; return new Uint8Array(); } };
  for (const value of [0, -1, 1.5, NaN, Infinity, -Infinity, 11, Number.MAX_SAFE_INTEGER, "1", null]) {
    const invalid = { ...server };
    Object.defineProperty(invalid, "maxSharedMemoBlocksPerRequest", { value });
    await assert.rejects(createWallet({ network: "regtest", server: invalid, storage: f.storage, onLoadProgress: () => { calls++; } }), code("unknown"));
    assert.throws(() => createWasmClient({ network: "regtest", transport: invalid }), code("unknown"));
  }
  for (const value of ["auto", "", null, 1, false, {}]) {
    const options = { network: "regtest", server, storage: f.storage, onLoadProgress: () => { calls++; } } satisfies Parameters<typeof createWallet>[0];
    Object.defineProperty(options, "submissionPolicy", { value });
    await assert.rejects(createWallet(options), code("unknown"));
    const raw = { network: "regtest", transport: server } satisfies Parameters<typeof createWasmClient>[0];
    Object.defineProperty(raw, "submissionPolicy", { value });
    assert.throws(() => createWasmClient(raw), code("unknown"));
  }
  assert.equal(calls, 0);
  await existing.create({ birthday: 8 });
  assert.equal((await existing.getWallet()).scannedHeight, 10);
});

test("memo request and explicit submission policy capabilities are frozen root contracts", () => {
  assert.equal(SDK_TRANSPORT_CAPABILITIES.maxSharedMemoBlocksPerRequest, true);
  assert.equal(SDK_TRANSPORT_CAPABILITIES.submissionPolicy, true);
  assert.equal(SDK_TRANSPORT_CAPABILITIES.submitContext, true);
  assert.equal(Object.isFrozen(SDK_TRANSPORT_CAPABILITIES), true);
});

// Transport/lifecycle regressions use the public facade and synthetic signed bytes.
// The engine-owned selection is separately covered by real WASM/browser fixtures.
test("explicit saved-byte retry survives cold reopen without signing or enabling sync resubmission", async t => {
  const f = fixture(t), submitted: string[] = [];
  let acknowledge = false;
  const server = { kind: "fixture", label: "offline", tip: async () => 10,
    blocks: async () => new Uint8Array(),
    submit: async (hex: string, _signal?: AbortSignal, context?: { readonly txid: string }) => {
      assert.deepEqual(context, { txid }); submitted.push(hex);
      if (!acknowledge) throw new TypeError("synthetic lost acknowledgement");
      return txid;
    } };
  const first = await f.open({ server, submissionPolicy: "explicit" });
  await first.create({ birthday: 8 }); await first.unlock(words);
  await assert.rejects(first.send("fixture", "0.00005"), code("broadcast_failed"));
  await first.close();
  const wallet = await f.open({ server, submissionPolicy: "explicit" });
  await wallet.load();
  assert.equal(wallet.hasSpendingSeed(), false);
  acknowledge = true;
  assert.deepEqual(await wallet.retryPendingSubmission(txid), { txid });
  assert.deepEqual(submitted, ["synthetic-transaction", "synthetic-transaction"]);
  assert.equal((await wallet.pending())[0]?.txid, txid);
  assert.equal((await wallet.getWallet()).balance.totalAvailable, 0);
  await wallet.sync();
  assert.equal(submitted.length, 2);
  await wallet.close();
  const reopened = await f.open({ server, submissionPolicy: "explicit" });
  await reopened.load(); await reopened.sync();
  assert.equal(submitted.length, 2, "an explicit retry must not change the later sync policy");
  assert.equal((await reopened.pending())[0]?.txid, txid);
});

for (const refusal of [false, true]) {
  test(`${refusal ? "refused" : "unknown"} saved-byte retry preserves the original receipt and reservations`, async t => {
    const f = fixture(t), submitted: string[] = [];
    let retrying = false;
    const wallet = await f.open({ submissionPolicy: "explicit", server: {
      kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
      submit: async (hex, _signal, context) => {
        submitted.push(hex); assert.deepEqual(context, { txid });
        if (retrying && refusal) throw new BroadcastRejection(17, "synthetic refusal");
        throw new TypeError("synthetic lost acknowledgement");
      },
    } });
    await wallet.create({ birthday: 8 }); await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
    const before = await readSavedSnapshotRecord(); assert.ok(before);
    retrying = true;
    await assert.rejects(wallet.retryPendingSubmission(txid), error =>
      code(refusal ? "broadcast_rejected" : "broadcast_failed")(error) && error instanceof WalletError && error.txid === txid);
    const after = await readSavedSnapshotRecord(); assert.ok(after);
    assert.deepEqual(after.bytes, before.bytes);
    assert.deepEqual(submitted, ["synthetic-transaction", "synthetic-transaction"]);
    assert.equal((await wallet.pending())[0]?.txid, txid);
    assert.equal((await wallet.getWallet()).balance.totalAvailable, 0);
    await wallet.sync(); assert.equal(submitted.length, 2);
  });
}

test("invalid retry identity and pre-cancellation refuse before storage or providers", async t => {
  const f = fixture(t); let reads = 0, writes = 0, submissions = 0;
  const storage = { available: true, transaction: (...args: Parameters<typeof f.storage.transaction>) => {
    if (args[0] === "readonly") reads++; else writes++;
    return f.storage.transaction(...args);
  } };
  const wallet = await f.open({ storage, submissionPolicy: "explicit", server: {
    kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
    submit: async () => { submissions++; throw new TypeError("synthetic lost acknowledgement"); },
  } });
  await wallet.create({ birthday: 8 }); await wallet.unlock(words);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
  reads = 0; writes = 0; submissions = 0;
  for (const value of ["", "AB".repeat(32), "a".repeat(63), "g".repeat(64), txid + " "])
    await assert.rejects(wallet.retryPendingSubmission(value), code("unknown"));
  const controller = new AbortController(); controller.abort();
  await assert.rejects(wallet.retryPendingSubmission(txid, { signal: controller.signal }), code("cancelled"));
  assert.equal(reads, 0); assert.equal(writes, 0); assert.equal(submissions, 0);
  await assert.rejects(wallet.retryPendingSubmission("cd".repeat(32)), code("not_found"));
  assert.equal(submissions, 0);
});

test("failed retry checkpoint refuses network and preserves previously saved bytes", async t => {
  const f = fixture(t); let refuseWrites = false, submissions = 0;
  const storage = { available: true, transaction: (...args: Parameters<typeof f.storage.transaction>) => {
    if (refuseWrites && args[0] === "readwrite") throw new WalletError("storage_full", "synthetic quota");
    return f.storage.transaction(...args);
  } };
  const wallet = await f.open({ storage, submissionPolicy: "explicit", server: {
    kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
    submit: async () => { submissions++; throw new TypeError("synthetic lost acknowledgement"); },
  } });
  await wallet.create({ birthday: 8 }); await wallet.unlock(words);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
  const before = await readSavedSnapshotRecord(); assert.ok(before);
  refuseWrites = true;
  await assert.rejects(wallet.retryPendingSubmission(txid), code("storage_full"));
  refuseWrites = false;
  const after = await readSavedSnapshotRecord(); assert.ok(after);
  assert.deepEqual(after.bytes, before.bytes);
  assert.equal(submissions, 1);
});

for (const cancel of ["caller", "lock", "close"] as const) {
  test(`${cancel} aborts an uncooperative saved-byte retry and fences late acknowledgement`, async t => {
    const f = fixture(t); let retrying = false, submissions = 0, providerSignal: AbortSignal | undefined;
    let entered!: () => void, finish!: (value: string) => void;
    const started = new Promise<void>(resolve => { entered = resolve; });
    const acknowledgement = new Promise<string>(resolve => { finish = resolve; });
    const wallet = await f.open({ submissionPolicy: "explicit", server: {
      kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
      submit: async (_hex, signal) => {
        submissions++;
        if (!retrying) throw new TypeError("synthetic lost acknowledgement");
        providerSignal = signal; entered(); return acknowledgement;
      },
    } });
    await wallet.create({ birthday: 8 }); await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
    const before = await readSavedSnapshotRecord(); assert.ok(before);
    retrying = true;
    const controller = new AbortController();
    const retry = wallet.retryPendingSubmission(txid, { signal: controller.signal });
    const rejected = assert.rejects(retry, error => code("broadcast_failed")(error) && error instanceof WalletError && error.txid === txid);
    await started;
    if (cancel === "caller") controller.abort();
    else if (cancel === "lock") wallet.lock();
    else await wallet.close();
    await rejected;
    assert.equal(providerSignal?.aborted, true);
    if (cancel !== "caller") assert.equal(controller.signal.aborted, false);
    finish(txid); await new Promise(resolve => setImmediate(resolve));
    if (cancel === "close") {
      const reopened = await f.open({ submissionPolicy: "explicit" });
      await reopened.load();
      assert.equal((await reopened.pending())[0]?.txid, txid);
    }
    const after = await readSavedSnapshotRecord(); assert.ok(after);
    assert.deepEqual(after.bytes, before.bytes);
    assert.equal(submissions, 2);
    if (cancel !== "close") assert.equal((await wallet.pending())[0]?.txid, txid);
  });
}

test("concurrent explicit retries share the spend exclusion and never duplicate the request", async t => {
  const f = fixture(t); let retrying = false, submissions = 0;
  let entered!: () => void, finish!: (value: string) => void;
  const started = new Promise<void>(resolve => { entered = resolve; });
  const acknowledgement = new Promise<string>(resolve => { finish = resolve; });
  const wallet = await f.open({ submissionPolicy: "explicit", server: {
    kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
    submit: async () => { submissions++; if (!retrying) throw new TypeError("synthetic lost acknowledgement"); entered(); return acknowledgement; },
  } });
  await wallet.create({ birthday: 8 }); await wallet.unlock(words);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
  retrying = true;
  const first = wallet.retryPendingSubmission(txid); await started;
  await assert.rejects(wallet.retryPendingSubmission(txid), code("busy"));
  assert.equal(submissions, 2);
  finish(txid); assert.deepEqual(await first, { txid });
  assert.equal((await wallet.pending())[0]?.txid, txid);
});

test("explicit retry capability is an immutable public contract", () => {
  assert.equal(SDK_TRANSPORT_CAPABILITIES.pendingSubmissionRetry, true);
  assert.equal(Object.isFrozen(SDK_TRANSPORT_CAPABILITIES), true);
});

for (const acknowledgement of ["cd".repeat(32), "", txid.toUpperCase(), undefined]) {
  test(`saved-byte retry treats ${acknowledgement === undefined ? "missing" : acknowledgement === "" ? "empty" : acknowledgement === txid.toUpperCase() ? "noncanonical" : "different"} acknowledgement as unknown`, async t => {
    const f = fixture(t); let submissions = 0;
    const server = { kind: "fixture", label: "offline", tip: async () => 10,
      blocks: async () => new Uint8Array(), submit: async () => { submissions++; throw new TypeError("synthetic lost acknowledgement"); } };
    const wallet = await f.open({ server, submissionPolicy: "explicit" });
    await wallet.create({ birthday: 8 }); await wallet.unlock(words);
    await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
    const before = await readSavedSnapshotRecord(); assert.ok(before);
    // A malformed external provider can violate its declared return type.
    Object.defineProperty(server, "submit", { value: async (_hex: string, _signal?: AbortSignal, context?: { readonly txid: string }) => {
      submissions++; assert.deepEqual(context, { txid }); return acknowledgement;
    } });
    await assert.rejects(wallet.retryPendingSubmission(txid), error =>
      code("broadcast_failed")(error) && error instanceof WalletError && error.txid === txid);
    const after = await readSavedSnapshotRecord(); assert.ok(after);
    assert.deepEqual(after.bytes, before.bytes);
    assert.equal((await wallet.pending())[0]?.txid, txid);
    assert.equal((await wallet.getWallet()).balance.totalAvailable, 0);
    assert.equal(submissions, 2);
  });
}

test("retry requires the exact returned ID even for a textual already-known exception", async t => {
  const f = fixture(t); let retrying = false;
  const wallet = await f.open({ submissionPolicy: "explicit", server: {
    kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
    submit: async () => {
      if (retrying) throw new BroadcastRejection(-27, "transaction already in mempool");
      throw new TypeError("synthetic lost acknowledgement");
    },
  } });
  await wallet.create({ birthday: 8 }); await wallet.unlock(words);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
  retrying = true;
  await assert.rejects(wallet.retryPendingSubmission(txid), error =>
    code("broadcast_failed")(error) && error instanceof WalletError && error.txid === txid);
  assert.equal((await wallet.pending())[0]?.txid, txid);
  assert.equal((await wallet.getWallet()).balance.totalAvailable, 0);
});

test("another tab winning the retry checkpoint prevents submission and preserves its newer snapshot", async t => {
  const f = fixture(t); let conflict = false, submissions = 0;
  let wallet: Wallet;
  let winningBytes: Uint8Array | undefined;
  const storage = { available: true, transaction: async (...args: Parameters<typeof f.storage.transaction>) => {
    if (conflict && args[0] === "readwrite") {
      conflict = false;
      const record = await readSavedSnapshotRecord(); assert.ok(record);
      const state = decode(record.bytes);
      winningBytes = encode({ ...state, address: "other-tab-address" });
      await saveWalletSnapshot(winningBytes, { ...(await wallet.getWallet()), unifiedAddress: "other-tab-address" },
        () => true, record.generation, record.key);
    }
    return f.storage.transaction(...args);
  } };
  wallet = await f.open({ storage, submissionPolicy: "explicit", server: {
    kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
    submit: async () => { submissions++; throw new TypeError("synthetic lost acknowledgement"); },
  } });
  await wallet.create({ birthday: 8 }); await wallet.unlock(words);
  await assert.rejects(wallet.send("fixture", "0.00005"), code("broadcast_failed"));
  conflict = true;
  await assert.rejects(wallet.retryPendingSubmission(txid), code("wallet_changed"));
  assert.equal(submissions, 1);
  const after = await readSavedSnapshotRecord(); assert.ok(after); assert.ok(winningBytes);
  assert.deepEqual(after.bytes, winningBytes);
  assert.equal((await wallet.getWallet()).unifiedAddress, "other-tab-address");
  assert.equal((await wallet.pending())[0]?.txid, txid);
});

test("public transparent mode setter rejects automatic shielding before changing wallet state", async t => {
  const f = fixture(t);
  const wallet = await f.open({ autoShield: true, unlockPolicy: "each-spend", server: {
    kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(), submit: async () => txid,
    transparentPir: { sourceId: "11".repeat(32), exchange: async () => { throw new Error("unexpected_private_query"); } },
  } });
  await wallet.create({ birthday: 8 });
  const before = await readSavedSnapshotRecord();
  assert.equal((await wallet.getWallet()).transparentScanStatus, "off");
  assert.throws(() => wallet.setTransparentScan("pir"), /Automatic shielding is not enabled for publisher-provided PIR history/);
  assert.equal((await wallet.getWallet()).transparentScanStatus, "off");
  assert.ok(JSON.stringify(await readSavedSnapshotRecord()) === JSON.stringify(before), "rejected mode change must preserve saved wallet state");
});

function privatePolicyFixture(t: TestContext) {
  const f = fixture(t);
  let outcome = "history_metadata_limit";
  let delayed: (() => Promise<Uint8Array>) | undefined;
  f.onPrivateTransparent(operation => {
    if (operation === "begin") return encode({ height: 10, hash: "22".repeat(32) });
    if (operation === "advance") return delayed ? delayed() : encode({ kind: "stopped", status: outcome === "complete" ? { kind: "complete" } : { kind: "incomplete", reason: outcome } });
    if (operation === "status") return encode({ completion: "complete-for-enrolled-scope" });
    return new Uint8Array();
  });
  return { f, complete: () => { outcome = "complete"; }, delay: (value: () => Promise<Uint8Array>) => { delayed = value; }, clearDelay: () => { delayed = undefined; },
    open: () => f.open({ transparentScan: "pir", server: {
      kind: "fixture", label: "offline", tip: async () => 10, blocks: async () => new Uint8Array(),
      transparentPir: { sourceId: "11".repeat(32), exchange: async () => { throw new Error("unexpected_private_query"); } },
    } }),
  };
}
test("public private-history limit publishes unavailable and clears on mode changes and successful retry", async t => {
  const f = privatePolicyFixture(t), wallet = await f.open();
  await wallet.create({ birthday: 8 });
  const limited = await wallet.sync();
  assert.equal(limited.transparentScanStatus, "unavailable");
  assert.equal(limited.transparentScanFailure, "history-metadata-limit");
  for (const mode of ["off", "compact"] as const) {
    wallet.setTransparentScan(mode);
    assert.equal((await wallet.getWallet()).transparentScanFailure, undefined);
    wallet.setTransparentScan("pir");
    assert.equal((await wallet.getWallet()).transparentScanFailure, undefined);
    await wallet.sync();
  }
  f.complete();
  const complete = await wallet.sync();
  assert.equal(complete.transparentScanStatus, "complete");
  assert.equal(complete.transparentScanFailure, undefined);
});
test("old metadata-limit response cannot publish after disabling and re-enabling the same owner", async t => {
  const f = privatePolicyFixture(t), wallet = await f.open();
  await wallet.create({ birthday: 8 });
  let enter!: () => void, finish!: (value: Uint8Array) => void;
  const entered = new Promise<void>(resolve => { enter = resolve; });
  f.delay(() => { enter(); return new Promise<Uint8Array>(resolve => { finish = resolve; }); });
  const old = wallet.sync();
  await entered;
  wallet.setTransparentScan("off"); wallet.setTransparentScan("pir");
  finish(encode({ kind: "stopped", status: { kind: "incomplete", reason: "history_metadata_limit" } }));
  await assert.rejects(old, /Privacy settings changed/);
  assert.equal((await wallet.getWallet()).transparentScanFailure, undefined);
  assert.equal((await wallet.getWallet()).transparentScanStatus, "scanning");
  f.clearDelay(); f.complete();
  assert.equal((await wallet.sync()).transparentScanStatus, "complete");
});
test("metadata policy limit does not auto-retry private history but still follows memos and new blocks", async t => {
  const clock = autoSyncClock(t), f = fixture(t); let tip = 10;
  const wallet = await f.open({ autoSync: true, server: { kind: "fixture", label: "offline", tip: async () => tip, blocks: async () => new Uint8Array() } });
  await wallet.create({ birthday: 1 });
  const snapshot = { ...await wallet.getWallet(), transparentScanStatus: "unavailable" as const, transparentScanFailure: "history-metadata-limit" as const, memoFetchStatus: "off" as const };
  t.mock.method(wallet, "getWallet", async () => snapshot);
  let syncs = 0; t.mock.method(wallet, "sync", async () => { syncs++; return snapshot; });
  await clock.tick(); assert.equal(syncs, 0);
  wallet.setMemoFetch("auto"); Object.assign(snapshot, { memoFetchStatus: "unavailable" });
  await clock.tick(); assert.equal(syncs, 1);
  Object.assign(snapshot, { memoFetchStatus: "off" }); tip = 11;
  await clock.tick(); assert.equal(syncs, 2);
});

test("replacing the wallet source clears the previous history policy failure", async t => {
  const f = privatePolicyFixture(t), wallet = await f.open();
  await wallet.create({ birthday: 8 });
  assert.equal((await wallet.sync()).transparentScanFailure, "history-metadata-limit");
  const replaced = await wallet.restore(words, { birthday: 8, replace: true });
  assert.equal(replaced.transparentScanFailure, undefined);
  assert.equal((await wallet.getWallet()).transparentScanFailure, undefined);
});
for (const action of ["lock", "replace"] as const) {
  test(`${action === "lock" ? "cancelled lock" : action} wins over an old metadata policy stop`, async t => {
    const f = privatePolicyFixture(t), wallet = await f.open();
    await wallet.create({ birthday: 8 });
    let enter!: () => void, finish!: (value: Uint8Array) => void;
    const entered = new Promise<void>(resolve => { enter = resolve; });
    f.delay(() => { enter(); return new Promise<Uint8Array>(resolve => { finish = resolve; }); });
    const old = wallet.sync();
    const rejected = assert.rejects(old, /cancelled|aborted|AbortError/i);
    await entered;
    if (action === "lock") wallet.cancelSync();
    const changed = action === "lock" ? wallet.lock() : wallet.restore(words, { birthday: 8, replace: true });
    finish(encode({ kind: "stopped", status: { kind: "incomplete", reason: "history_metadata_limit" } }));
    await changed; await rejected;
    assert.equal((await wallet.getWallet()).transparentScanFailure, undefined);
  });
}
