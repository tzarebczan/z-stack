import assert from "node:assert/strict";
import { test } from "node:test";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  DEFAULT_WASM_THREADS,
  MAX_WASM_THREADS,
  REGTEST_FAUCET_MNEMONIC,
  REGTEST_FAUCET_TRANSPARENT,
  cryptoSmoke,
  defaultThreadCount,
  threadsForCores,
  deriveAccount,
  generateMnemonic,
  initialize,
  parseAddress,
  zip321Uri,
  zip321UriMany,
} from "../src/lab.ts";

const gen = join(dirname(fileURLToPath(import.meta.url)), "..", "src", "generated", "z_wasm_bg.wasm");

test("thread defaults use one worker per physical core, capped", () => {
  assert.equal(MAX_WASM_THREADS, 32);
  assert.equal(DEFAULT_WASM_THREADS, 16);
  const cases: [number, number][] = [[0, 1], [1, 1], [4, 4], [8, 8], [12, 8], [16, 8], [20, 10], [24, 12], [32, 16], [64, 16]];
  for (const [logical, workers] of cases) assert.equal(threadsForCores(logical), workers, `${logical} logical cores`);
  assert.ok(defaultThreadCount() >= 1);
  assert.ok(defaultThreadCount() <= DEFAULT_WASM_THREADS);
});

test("wasm keys: mnemonic, derive, parse, zip321", async (t) => {
  if (!existsSync(gen)) {
    t.skip("run pnpm build:wasm first");
    return;
  }
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  assert.equal(cryptoSmoke(), "zakura-orchard linked");
  const words = generateMnemonic();
  assert.equal(words.split(/\s+/).length, 24);
  const acct = deriveAccount(words, "regtest", 0);
  assert.equal(acct.network, "regtest");
  assert.ok(acct.unifiedAddress.startsWith("uregtest1"));
  assert.equal(parseAddress(acct.unifiedAddress).kind, "unified");
  assert.ok(zip321Uri(acct.unifiedAddress).startsWith("zcash:uregtest1"));

  const faucet = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0);
  assert.equal(faucet.transparentAddress, REGTEST_FAUCET_TRANSPARENT);
  // Every address the SDK derives is valid for its own network.
  const { isValidAddress } = await import("../src/runtime.ts");
  for (const net of ["mainnet", "testnet", "regtest"] as const) {
    const a = deriveAccount(words, net, 0);
    assert.ok(isValidAddress(a.unifiedAddress, net), `${net} UA`);
    assert.ok(isValidAddress(a.transparentAddress!, net), `${net} transparent`);
  }
  assert.ok(!isValidAddress(deriveAccount(words, "testnet", 0).unifiedAddress, "regtest"), "testnet UAs are not regtest");
  assert.ok(!isValidAddress(deriveAccount(words, "mainnet", 0).transparentAddress!, "regtest"));
});

test("initialize accepts explicit effective defaults and rejects real configuration changes", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  const options = { wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false };
  await assert.rejects(initialize({ ...options, regtestNu63Height:150, regtestNu7Height:149 }), /after NU6.3/);
  await assert.rejects(initialize({ ...options, regtestNu7Height:NaN }), /Invalid regtestNu7Height/);
  await initialize(options);
  await assert.rejects(initialize({ ...options, regtestNu7Height:1_000_001 }), /different regtestNu7Height/);
  await initialize({ ...options, preferMulticore: true, threads: defaultThreadCount(), regtestNu63Height: 1_000_000 });
  await assert.rejects(initialize({ ...options, preferMulticore: false }), /different preferMulticore/);
  await assert.rejects(initialize({ ...options, threads: defaultThreadCount() + 1 }), /different threads/);
});

test("a genesis rescan refreshes subtree roots even within the same scan session", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  const { createWallet } = await import("../src/create-wallet.ts");
  const { memoryWalletStorage } = await import("../src/storage.ts");
  let roots = 0;
  const wallet = await createWallet({ network: "regtest", storage: memoryWalletStorage(),
    prewarmProvingKey: false, server: {
      kind: "fixture", label: "offline", tip: async () => 2, blocks: async () => new Uint8Array(),
      subtreeRoots: async () => { roots++; return []; },
    } });
  t.after(() => wallet.close());
  await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1 });
  // Empty fixture blocks stop the scan after its real engine's root setup.
  await assert.rejects(wallet.sync());
  assert.equal(roots, 2);
  await assert.rejects(wallet.sync());
  assert.equal(roots, 2, "an unchanged session should reuse the root cache");
  await wallet.rescan({ birthday: 1 });
  await assert.rejects(wallet.sync());
  assert.equal(roots, 4, "a reset must fetch roots again for the new engine state");
});

test("wasm wallet: snapshot, history, orchard prove", async (t) => {
  if (!existsSync(gen)) {
    t.skip("run pnpm build:wasm first");
    return;
  }
  const { createWasmClient, wasmCapabilities } = await import("../src/wasm-client.ts");
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  const caps = wasmCapabilities();
  assert.equal(caps.keys, true);
  assert.equal(caps.sync, true);
  assert.equal(caps.history, true);
  assert.equal(caps.prove, true);
  assert.equal(caps.transparentScan, true);
  assert.equal(caps.transparentOutputs, true);
  assert.equal(caps.multicore, false);
  assert.equal(caps.simd, true);
  assert.ok((caps.threads ?? 1) >= 1);

  const client = createWasmClient({ unlockPolicy: "session",
    network: "regtest",
    transport: {
      kind: "mock",
      label: "mock",
      tip: async () => 2,
      blocks: async () => new Uint8Array(),
    },
  });
  const w = await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1);
  assert.ok(w.unifiedAddress.startsWith("uregtest1"));
  assert.equal(w.birthdayHeight, 1);
  assert.equal(w.scannedHeight, 0);
  assert.equal((await client.history(10)).length, 0);
  assert.equal(client.unlockPolicy(), "session");
  assert.equal(client.hasSpendingSeed(), true);
  assert.equal(await client.supportsTransparentSend(), true);
  await assert.rejects(client.estimateTransparentFee(w.unifiedAddress, "0.1"), /bare transparent|P2PKH|P2SH/i);
  await assert.rejects(client.estimateTransparentFee(REGTEST_FAUCET_TRANSPARENT, "0"), /greater than zero/i);
  await assert.rejects(client.estimateTransparentFee(`zcash:${REGTEST_FAUCET_TRANSPARENT}?amount=0.1`, "0.1"), /transparent|invalid/i);
  await assert.rejects(client.estimateTransparentFee(REGTEST_FAUCET_TRANSPARENT, "0.1"), /sync required|insufficient/i);

  const ufvk = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0).ufvk;
  const view = await client.restoreUfvk(ufvk, "regtest", 1);
  assert.equal(view.viewOnly, true);
  assert.equal(client.hasSpendingSeed(), false);
  await client.attachSeed(REGTEST_FAUCET_MNEMONIC);
  assert.equal(client.hasSpendingSeed(), true);
  await assert.rejects(
    () => client.attachSeed("legal winner thank year wave sausage worth useful legal winner thank yellow"),
    /match|viewing key/i,
  );
  await assert.rejects(() => client.sync(), /empty compact-block blob|sync incomplete/i);
  await assert.rejects(() => client.shield(), /transparent scan|GetAddressUtxos|utxo/i);
  await assert.rejects(
    () => client.send("uregtest1abc", "0.0001"),
    /insufficient|sync required|invalid address|broadcast/i,
  );
  await assert.rejects(
    () => client.send(REGTEST_FAUCET_TRANSPARENT, "0.0001"),
    /transparent send is not supported|invalid address/i,
  );
  try {
    const max = await client.maxSend();
    assert.equal(max.maxSendZat, 0);
  } catch (e) {
    assert.match(String(e instanceof Error ? e.message : e), /cannot estimate|sync required|insufficient|no wasm/i);
  }
  const syncs: string[] = [];
  const off = client.on("sync", (e) => syncs.push(e.stage));
  off();
  assert.equal(typeof client.off, "function");
  const multi = zip321UriMany([
    { address: w.unifiedAddress, amountZec: "0.0001" },
    { address: REGTEST_FAUCET_TRANSPARENT, amountZec: "0.0001" },
  ]);
  await assert.rejects(
    () => client.send(multi, ""),
    /transparent send is not supported|invalid address/i,
  );
  const next = await client.nextAddress();
  assert.ok(next.unifiedAddress.startsWith("uregtest1"));
  assert.notEqual(next.unifiedAddress, w.unifiedAddress);

  const cleared = await client.resetScan();
  assert.equal(cleared.unifiedAddress, next.unifiedAddress);
  assert.equal(cleared.birthdayHeight, 1);
  assert.equal(cleared.scannedHeight, 0);
  assert.equal((await client.history(10)).length, 0);
  assert.equal(client.hasSpendingSeed(), true);
});

for (const failure of ["missing", "unavailable", "wrong-height", "wrong-network"] as const) {
  test(`strict custom transport refuses ${failure} birthday frontier before requesting blocks`, async t => {
    assert.ok(existsSync(gen), "build the production WASM engine before this boundary test");
    const { createWallet } = await import("../src/create-wallet.ts");
    const { memoryWalletStorage } = await import("../src/storage.ts");
    const storage = memoryWalletStorage();
    let blocks = 0;
    let frontierSignal: AbortSignal | undefined;
    const treeState = failure === "missing" ? undefined : async (height: number, signal?: AbortSignal) => {
      frontierSignal = signal;
      assert.equal(height, 1);
      if (failure === "unavailable") throw new Error("fixture frontier unavailable");
      return { height: failure === "wrong-height" ? 0 : 1, hash: "01".repeat(32),
        network: failure === "wrong-network" ? "testnet" : "regtest" };
    };
    const wallet = await createWallet({ network: "regtest", storage,
      requireBirthdayTreeState: true, prewarmProvingKey: false, memoFetch: "on-demand", transparentScan: "off",
      server: { kind: "custom-only", label: "custom fixture", tip: async () => 2,
        blocks: async () => { blocks++; throw new Error("unexpected blocks"); }, treeState } });
    t.after(() => wallet.close());
    await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 2 });
    const before = await wallet.getWallet();
    await assert.rejects(wallet.sync(), /requires a birthday tree state|required birthday tree state/);
    assert.equal(blocks, 0);
    if (failure !== "missing") assert.ok(frontierSignal);
    const after = await wallet.getWallet();
    assert.equal(after.scannedHeight, before.scannedHeight);
    assert.equal(after.unifiedAddress, before.unifiedAddress);
    assert.equal(after.treesReady, false);
    await wallet.close();
  });
}

test("strict custom transport cold restore and incremental sync use only supplied capabilities", async t => {
  assert.ok(existsSync(gen), "build the production WASM engine before this boundary test");
  const { createWallet } = await import("../src/create-wallet.ts");
  const { memoryWalletStorage } = await import("../src/storage.ts");
  const calls: string[] = [];
  let tip = 2;
  const block = (height: number) => {
    const protobuf = Uint8Array.from([16, height, 26, 32, ...Array(32).fill(height),
      34, 32, ...Array(32).fill(height - 1), 40, 1]);
    const frame = new Uint8Array(protobuf.length + 4);
    new DataView(frame.buffer).setUint32(0, protobuf.length, false);
    frame.set(protobuf, 4);
    return frame;
  };
  const wallet = await createWallet({ network: "regtest", storage: memoryWalletStorage(),
    requireBirthdayTreeState: true, verifyChainTip: true, prewarmProvingKey: false,
    memoFetch: "on-demand", transparentScan: "off", server: {
      kind: "custom-only", label: "custom fixture",
      tip: async signal => { assert.ok(signal); calls.push("tip"); return tip; },
      treeState: async (height, signal) => {
        assert.ok(signal); calls.push(`tree:${height}`);
        return { height, hash: "01".repeat(32), network: "regtest" };
      },
      blocks: async (start, end, signal) => {
        assert.ok(signal); calls.push(`blocks:${start}-${end}`);
        assert.equal(start, end);
        return block(start);
      },
    } });
  t.after(() => wallet.close());
  await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 2 });
  const cold = await wallet.sync();
  assert.equal(cold.scannedHeight, 2);
  assert.equal(cold.treesReady, true);
  assert.deepEqual(calls, ["tip", "tip", "tree:1", "blocks:2-2"]);
  calls.length = 0;
  tip = 3;
  const incremental = await wallet.sync();
  assert.equal(incremental.scannedHeight, 3);
  assert.deepEqual(calls, ["tip", "blocks:2-2", "blocks:3-3"]);
  await wallet.close();
});

test("real WASM upgrades a legacy cold snapshot and refuses an unrelated first block", async () => {
  assert.ok(existsSync(gen), "build the production WASM engine before this upgrade test");
  const { requireBindings } = await import("../src/wasm-client-runtime.ts");
  const Wallet = requireBindings().WasmWallet;
  const initial = Wallet.create("regtest", REGTEST_FAUCET_MNEMONIC, 12, 0);
  let bytes: Uint8Array;
  try {
    assert.equal(typeof initial.applyTreeState, "function");
    initial.applyTreeState?.(JSON.stringify({ height: 11, hash: `01${"00".repeat(31)}`, network: "regtest" }));
    bytes = initial.toSnapshot();
  } finally { initial.free?.(); }
  const legacy = JSON.parse(new TextDecoder().decode(bytes));
  legacy.blockHashes = {};
  const restored = Wallet.fromSnapshot(new TextEncoder().encode(JSON.stringify(legacy)));
  try {
    const snapshot = JSON.parse(new TextDecoder().decode(restored.toSnapshot()));
    assert.equal(snapshot.blockHashes["11"], `${"00".repeat(31)}01`);
    assert.equal(snapshot.unifiedAddress, legacy.unifiedAddress);
    assert.equal(snapshot.scannedHeight, 11);
    const protobuf = Uint8Array.from([16, 12, 26, 32, ...Array(32).fill(12),
      34, 32, ...Array(32).fill(3), 40, 1]);
    const frame = new Uint8Array(protobuf.length + 4);
    new DataView(frame.buffer).setUint32(0, protobuf.length, false);
    frame.set(protobuf, 4);
    assert.throws(() => restored.applyCompactBlocks(frame));
    assert.equal(restored.scannedHeight(), 11);
  } finally { restored.free?.(); }
});

test("real WASM refuses omitted and short compact hashes before snapshot mutation", async () => {
  assert.ok(existsSync(gen), "build the production WASM engine before this boundary test");
  const { requireBindings } = await import("../src/wasm-client-runtime.ts");
  const Wallet = requireBindings().WasmWallet;
  const initial = Wallet.create("regtest", REGTEST_FAUCET_MNEMONIC, 12, 0);
  let seeded: Uint8Array;
  try {
    initial.applyTreeState?.(JSON.stringify({ height: 11, hash: "01".repeat(32), network: "regtest" }));
    seeded = initial.toSnapshot();
  } finally { initial.free?.(); }
  for (const length of [0, 1, 31, 33]) {
    for (const parent of [false, true]) {
      const hash = Array(parent ? 32 : length).fill(12);
      const previous = Array(parent ? length : 32).fill(1);
      const protobuf = Uint8Array.from([16, 12, 26, hash.length, ...hash,
        34, previous.length, ...previous, 40, 1]);
      const frame = new Uint8Array(protobuf.length + 4);
      new DataView(frame.buffer).setUint32(0, protobuf.length, false);
      frame.set(protobuf, 4);
      const wallet = Wallet.fromSnapshot(seeded);
      try {
        assert.throws(() => wallet.applyCompactBlocks(frame), /hash and prev_hash must be 32 bytes/);
        assert.deepEqual(wallet.toSnapshot(), seeded);
      } finally { wallet.free?.(); }
    }
  }
});

for (const failure of ["frontier", "subtree-root"] as const) {
  test(`strict ${failure} decode failure preserves saved wallet and a corrected ordinary retry succeeds`, async t => {
  assert.ok(existsSync(gen), "build the production WASM engine before this recovery test");
  const { createWallet } = await import("../src/create-wallet.ts");
  const { memoryWalletStorage } = await import("../src/storage.ts");
  const { readSavedSnapshot } = await import("../src/snapshot-storage.ts");
  let malformed = true;
  let blocks = 0;
  const wallet = await createWallet({ network: "regtest", storage: memoryWalletStorage(),
    requireBirthdayTreeState: true, memoFetch: "on-demand", transparentScan: "off", prewarmProvingKey: false,
    server: { kind: "custom-only", label: "retry fixture", tip: async () => 2,
      treeState: async height => ({ height, network: "regtest", hash: "01".repeat(32), saplingTree: malformed && failure === "frontier" ? "zz" : "" }),
      subtreeRoots: async protocol => malformed && failure === "subtree-root" && protocol === "sapling" ? [{ completingHeight: 1, rootHash: "ff".repeat(32) }] : [],
      blocks: async () => {
        blocks++;
        const bytes = Uint8Array.from([16, 2, 26, 32, ...Array(32).fill(2), 34, 32, ...Array(32).fill(1), 40, 1]);
        const frame = new Uint8Array(bytes.length + 4);
        new DataView(frame.buffer).setUint32(0, bytes.length, false);
        frame.set(bytes, 4);
        return frame;
      },
    } });
  t.after(() => wallet.close());
  const initial = await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 2 });
  const saved = await readSavedSnapshot();
  assert.ok(saved);
  await assert.rejects(wallet.sync(), failure === "frontier" ? /required birthday tree state/ : /root|invalid|decode/i);
  assert.equal(blocks, 0);
  const afterFailure = await readSavedSnapshot();
  assert.ok(afterFailure && saved.every((byte, index) => afterFailure[index] === byte) && afterFailure.length === saved.length, "failed frontier changed saved wallet bytes");
  malformed = false;
  const restored = await wallet.sync();
  assert.equal(restored.scannedHeight, 2);
  assert.equal(restored.unifiedAddress, initial.unifiedAddress);
  assert.equal(restored.birthdayHeight, 2);
  assert.equal(blocks, 1);
});
}
