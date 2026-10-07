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
  await initialize(options);
  await initialize({ ...options, preferMulticore: true, threads: defaultThreadCount(), regtestNu63Height: 1_000_000 });
  await assert.rejects(initialize({ ...options, preferMulticore: false }), /different preferMulticore/);
  await assert.rejects(initialize({ ...options, threads: defaultThreadCount() + 1 }), /different threads/);
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
  const w = await client.restore(REGTEST_FAUCET_MNEMONIC, "regtest", "auto");
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
