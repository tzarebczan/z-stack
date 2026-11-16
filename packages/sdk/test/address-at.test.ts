import assert from "node:assert/strict";
import { test } from "node:test";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { isWalletError } from "@z-stack/core";
import { REGTEST_FAUCET_MNEMONIC, deriveAccount, initialize } from "../src/lab.ts";
import { inspectAddress, unifiedAddressAt, unifiedAddressForSet } from "../src/runtime.ts";

const gen = join(dirname(fileURLToPath(import.meta.url)), "..", "src", "generated", "z_wasm_bg.wasm");
const invalidIndex = (error: unknown) => isWalletError(error) && error.code === "invalid_diversifier_index";

test("unifiedAddressAt is deterministic, distinct per index and matches the default at 0", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  const { ufvk } = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0);
  const at = (index: number | bigint) => unifiedAddressAt(ufvk, "regtest", "orchard", index);

  assert.deepEqual(at(0), { address: unifiedAddressForSet(ufvk, "regtest", "orchard"), diversifierIndex: 0n });
  assert.deepEqual(at(5), at(5n));
  const indices = [0n, 1n, 2n, 99n, 2n ** 32n, 2n ** 88n - 1n];
  const addresses = indices.map(index => at(index));
  assert.equal(new Set(addresses.map(found => found.address)).size, indices.length);
  for (const [i, found] of addresses.entries()) {
    assert.equal(found.diversifierIndex, indices[i]);
    assert.equal(inspectAddress(found.address).receiverSet, "orchard");
  }
  for (const bad of [-1, 1.5, Number.MAX_SAFE_INTEGER + 2, 2n ** 88n, -1n]) assert.throws(() => at(bad), invalidIndex);

  // Sapling-invalid indices: error by default, or the next valid index on request.
  const invalid = [...Array(64).keys()].find(index => {
    try { unifiedAddressAt(ufvk, "regtest", "shielded", index); return false; }
    catch (error) { assert.ok(invalidIndex(error)); return true; }
  });
  assert.notEqual(invalid, undefined);
  const next = unifiedAddressAt(ufvk, "regtest", "shielded", invalid!, { invalidIndex: "next" });
  assert.ok(next.diversifierIndex > BigInt(invalid!));
  assert.deepEqual(unifiedAddressAt(ufvk, "regtest", "shielded", next.diversifierIndex), next);
  assert.equal(inspectAddress(next.address).receiverSet, "shielded");

  // Transparent receivers need an explicit opt-in.
  assert.throws(() => unifiedAddressAt(ufvk, "regtest", "full", 0), TypeError);
  const full = unifiedAddressAt(ufvk, "regtest", "full", 0, { allowTransparent: true, invalidIndex: "next" });
  assert.equal(inspectAddress(full.address).receiverSet, "full");
  assert.throws(() => unifiedAddressAt(ufvk, "regtest", "full", 2 ** 31, { allowTransparent: true }), invalidIndex);
});

test("unifiedAddressAt writes nothing and leaves the wallet's next address unchanged", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  const { createWallet } = await import("../src/create-wallet.ts");
  const { memoryWalletStorage } = await import("../src/storage.ts");
  const base = memoryWalletStorage();
  let writes = 0;
  const storage = { ...base, transaction: ((mode, body, options) => {
    if (mode === "readwrite") writes++;
    return base.transaction(mode, body, options);
  }) as typeof base.transaction };
  const wallet = await createWallet({ network: "regtest", storage, prewarmProvingKey: false,
    server: { kind: "fixture", label: "offline", tip: async () => 2, blocks: async () => new Uint8Array() } });
  t.after(() => wallet.close());
  const restored = await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1 });
  const before = writes;
  for (let index = 0; index < 32; index++) unifiedAddressAt(restored.ufvk!, "regtest", "orchard", index);
  assert.equal(writes, before);
  assert.equal((await wallet.getWallet()).unifiedAddress, restored.unifiedAddress);
});
