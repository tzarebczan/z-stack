import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { serialize } from "node:v8";
import { isWalletError, type WalletErrorCode, type WalletSnapshot } from "@z-stack/core";
import { indexedDbWalletStorage, type WalletStorage } from "../src/storage.ts";
import { decryptWalletStore, encryptWalletStore, encryptedWalletStorage, eraseWalletStore, inspectWalletStore, plainWalletStorage } from "../src/store-encryption.ts";
import { peekSavedSnapshot, readSavedSnapshot, readWalletGeneration, saveWalletSnapshot } from "../src/snapshot-storage.ts";
import { useWalletStorage } from "../src/wallet-storage.ts";
import { checkWalletStorageAdapter } from "../src/lab/adapter-conformance.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";

const gen = join(dirname(fileURLToPath(import.meta.url)), "..", "src", "generated", "z_wasm_bg.wasm");

// Regtest fixture values; the txid is a display-order hex string.
const UFVK = "uviewregtest1fixtureviewingkeyqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
const ADDRESS = "uregtest1fixtureaddressqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
const TXID = "7d3a2c8e4b1f09a6c5e2d7b8f4a3c1e0d9b6a5f2e8c7d4b3a1f0e9d8c7b6a5f4";

const preview = (height = 10) => ({
  network: "regtest", server: "", birthdayHeight: 1, scannedHeight: height,
  unifiedAddress: ADDRESS, ufvk: UFVK, transparentAddress: null, zip321: `zcash:${ADDRESS}`,
  balance: { totalAvailable: height, orchardAvailable: height },
} as WalletSnapshot);

/** Opaque wallet bytes over the chunking threshold, carrying fixture identifiers. */
function walletBytes(fill = 7): Uint8Array {
  const text = new TextEncoder().encode(JSON.stringify({ ufvk: UFVK, address: ADDRESS, txs: [TXID] }));
  const bytes = new Uint8Array(160 * 1024).fill(fill);
  for (let offset = 0; offset + text.length < bytes.length; offset += 40 * 1024) bytes.set(text, offset);
  return bytes;
}

function installDb(t: TestContext) {
  const db = memoryIndexedDb();
  const previous = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "IDBKeyRange", { value: db.IDBKeyRange, configurable: true });
  t.after(() => {
    useWalletStorage();
    if (previous) Object.defineProperty(globalThis, "IDBKeyRange", previous);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  });
  return { db, storage: indexedDbWalletStorage({ factory: db.indexedDB }) };
}

const newKey = () => crypto.getRandomValues(new Uint8Array(32));
const code = (expected: WalletErrorCode) => (error: unknown) => isWalletError(error) && error.code === expected;

/** Search every persisted record's raw bytes (and its name) for fixture identifiers. */
function hits(records: Map<IDBValidKey, unknown>): string[] {
  const found: string[] = [];
  const needles = [UFVK, ADDRESS, TXID, "fixtureviewingkey", "fixtureaddress"].map(text => new TextEncoder().encode(text));
  for (const [name, value] of records) {
    const raw = new Uint8Array(serialize(value));
    for (const needle of needles) {
      if (String(name).includes(new TextDecoder().decode(needle))) found.push(`name ${String(name)}`);
      outer: for (let i = 0; i + needle.length <= raw.length; i++) {
        for (let j = 0; j < needle.length; j++) if (raw[i + j] !== needle[j]) continue outer;
        found.push(String(name));
        break;
      }
    }
  }
  return found;
}

test("an encrypted store round-trips wallet state and persists no plaintext wallet data", async t => {
  const { db, storage } = installDb(t);
  const key = newKey();
  useWalletStorage(encryptedWalletStorage(storage, key));
  const first = walletBytes();
  const saved = await saveWalletSnapshot(first, preview(), () => true);
  assert.deepEqual(await readSavedSnapshot(), first);
  const next = walletBytes(9);
  await saveWalletSnapshot(next, preview(20), () => true, undefined, saved);
  assert.deepEqual(await readSavedSnapshot(), next);
  const shown = await peekSavedSnapshot(() => null);
  assert.equal(shown?.unifiedAddress, ADDRESS);
  assert.equal(shown?.ufvk, UFVK);
  assert.equal(shown?.scannedHeight, 20);
  assert.equal(typeof await readWalletGeneration(), "string");

  assert.ok(db.records.size > 5);
  assert.deepEqual(hits(db.records), [], "no address, viewing key or txid in raw bytes or record names");
  for (const [name, value] of db.records) {
    if (name === "z-stack:encryption") continue;
    assert.ok(value && typeof value === "object" && "ct" in value, `${String(name)} is sealed`);
  }
  assert.deepEqual(await inspectWalletStore({ storage }), { exists: true, encrypted: true });

  // The same scan finds the identifiers in an unencrypted store.
  const plain = installDb(t);
  useWalletStorage(plain.storage);
  await saveWalletSnapshot(walletBytes(), preview(), () => true);
  assert.ok(hits(plain.db.records).length > 0);
  assert.deepEqual(await inspectWalletStore({ storage: plain.storage }), { exists: true, encrypted: false });
  assert.deepEqual(await inspectWalletStore({ storage: installDb(t).storage }), { exists: false, encrypted: false });
});

test("wrong or missing keys fail closed and never read as an empty wallet", async t => {
  const { db, storage } = installDb(t);
  useWalletStorage(encryptedWalletStorage(storage, newKey()));
  await saveWalletSnapshot(walletBytes(), preview(), () => true);
  const before = new Map(db.records);

  useWalletStorage(encryptedWalletStorage(storage, newKey()));
  await assert.rejects(readSavedSnapshot(), code("store_key_rejected"));
  await assert.rejects(saveWalletSnapshot(walletBytes(1), preview(), () => true), code("store_key_rejected"));

  useWalletStorage(storage);
  await assert.rejects(readSavedSnapshot(), code("store_locked"));
  await assert.rejects(peekSavedSnapshot(() => null), code("store_locked"));
  await assert.rejects(saveWalletSnapshot(walletBytes(1), preview(), () => true), code("store_locked"));
  assert.deepEqual(db.records, before, "failed opens and saves change nothing");

  // A key supplied for an existing unencrypted store is refused, not adopted.
  const plain = installDb(t);
  useWalletStorage(plain.storage);
  await saveWalletSnapshot(walletBytes(), preview(), () => true);
  useWalletStorage(encryptedWalletStorage(plain.storage, newKey()));
  await assert.rejects(readSavedSnapshot(), code("store_not_encrypted"));
  await assert.rejects(saveWalletSnapshot(walletBytes(1), preview(), () => true), code("store_not_encrypted"));
  // A write that reads nothing cannot start an encrypted store on top of unencrypted records.
  const vaultOnly = installDb(t);
  await vaultOnly.storage.transaction("readwrite", tx => tx.put("passkey.v1", { id: "fixture" }));
  const blind = encryptedWalletStorage(vaultOnly.storage, newKey());
  await assert.rejects(blind.transaction("readwrite", tx => tx.put("meta", { unlockPolicy: "session" })), code("store_not_encrypted"));
  assert.deepEqual([...vaultOnly.db.records.keys()], ["passkey.v1"]);

  assert.throws(() => encryptedWalletStorage(storage, new Uint8Array(16)), code("store_key_rejected"));
});

test("tampered, swapped, replayed or deleted records fail verification", async t => {
  const { db, storage } = installDb(t);
  useWalletStorage(encryptedWalletStorage(storage, newKey()));
  const key = await saveWalletSnapshot(walletBytes(), preview(), () => true);
  const original = new Map(db.records);
  const reset = () => { db.records.clear(); for (const [name, value] of original) db.records.set(name, structuredClone(value)); };
  const chunks = [...db.records.keys()].map(String).filter(name => name.includes(":c:"));
  assert.ok(chunks.length >= 2);

  (db.records.get(chunks[0]) as { ct: Uint8Array }).ct[5] ^= 1;
  await assert.rejects(readSavedSnapshot(), code("store_corrupt"));
  reset();
  const [a, b] = [db.records.get(chunks[0]), db.records.get(chunks[1])];
  db.records.set(chunks[0], b); db.records.set(chunks[1], a);
  await assert.rejects(readSavedSnapshot(), code("store_corrupt"));
  reset();
  db.records.delete("default");
  await assert.rejects(readSavedSnapshot(), code("store_corrupt"), "a deleted record is not an empty wallet");
  reset();

  // Replay: an older generation of an overwritten record, or of the head.
  const oldDefault = structuredClone(db.records.get("default"));
  const oldHead = structuredClone(db.records.get("z-stack:encryption:head"));
  await saveWalletSnapshot(walletBytes(3), preview(30), () => true, undefined, key);
  const current = new Map(db.records);
  db.records.set("default", oldDefault);
  await assert.rejects(readSavedSnapshot(), code("store_corrupt"));
  db.records.set("default", current.get("default"));
  db.records.set("z-stack:encryption:head", oldHead);
  await assert.rejects(readSavedSnapshot(), code("store_corrupt"));
  db.records.set("z-stack:encryption:head", current.get("z-stack:encryption:head"));
  assert.deepEqual(await readSavedSnapshot(), walletBytes(3));
});

test("migration encrypts and decrypts in place, atomically and resumably", async t => {
  const { db, storage } = installDb(t);
  useWalletStorage(storage);
  await saveWalletSnapshot(walletBytes(), preview(), () => true);
  await storage.transaction("readwrite", tx => tx.put("seed.enc", { v: 2, salt: "s", iv: "i", ct: "c" }));
  const plain = new Map(db.records);
  const key = newKey();

  // A write failing partway through encryption keeps the complete unencrypted store.
  db.failPutKey = "default";
  await assert.rejects(encryptWalletStore({ storage, key }));
  assert.deepEqual(db.records, plain);
  assert.deepEqual(await readSavedSnapshot(), walletBytes());

  await encryptWalletStore({ storage, key });
  await encryptWalletStore({ storage, key });
  await assert.rejects(encryptWalletStore({ storage, key: newKey() }), code("store_key_rejected"));
  assert.deepEqual(hits(db.records), []);
  assert.ok([...db.records.entries()].every(([name, value]) => name === "z-stack:encryption" || typeof value === "object" && value && "ct" in value),
    "no unencrypted copy remains");
  await assert.rejects(readSavedSnapshot(), code("store_locked"));
  useWalletStorage(encryptedWalletStorage(storage, key));
  assert.deepEqual(await readSavedSnapshot(), walletBytes());
  const encrypted = new Map(db.records);

  // A write failing partway through decryption keeps the complete encrypted store.
  db.failPutKey = "default";
  await assert.rejects(decryptWalletStore({ storage, key }));
  assert.deepEqual(db.records, encrypted);
  assert.deepEqual(await readSavedSnapshot(), walletBytes());
  await assert.rejects(decryptWalletStore({ storage, key: newKey() }), code("store_key_rejected"));
  assert.deepEqual(db.records, encrypted);

  await decryptWalletStore({ storage, key });
  await decryptWalletStore({ storage, key });
  assert.deepEqual(db.records, plain, "decryption restores the exact unencrypted records");
  useWalletStorage(storage);
  assert.deepEqual(await readSavedSnapshot(), walletBytes());
});

test("the unencrypted path writes exactly what it wrote before", async t => {
  const guarded = installDb(t), raw = installDb(t);
  const body = (storage: WalletStorage) => storage.transaction("readwrite", tx => {
    tx.put("default", { format: "fixture", preview: preview() });
    tx.put("snapshot:x", new Uint8Array([1, 2, 3]));
    tx.put("wallet:generation", "fixed");
    tx.get("default", value => tx.put("meta", { copied: value }));
  });
  await body(plainWalletStorage(guarded.storage));
  await body(raw.storage);
  assert.deepEqual(guarded.db.records, raw.db.records);
  assert.deepEqual(guarded.db.writes, raw.db.writes);
});

test("encrypted storage meets the adapter contract and erase needs no key", async t => {
  const { db, storage } = installDb(t);
  const key = newKey();
  const report = await checkWalletStorageAdapter({ open: () => encryptedWalletStorage(storage, key), dispose() {} });
  assert.equal(report.passed.length, 6);
  assert.deepEqual(hits(db.records), []);
  await eraseWalletStore({ storage });
  assert.deepEqual([...db.records.keys()], ["wallet:generation"]);
  assert.deepEqual(await inspectWalletStore({ storage }), { exists: false, encrypted: false });
});

test("createWallet with storageKey: saved wallet is sealed and opens only with its key", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  const { initialize, REGTEST_FAUCET_MNEMONIC, deriveAccount } = await import("../src/lab.ts");
  const { createWallet, forgetWallet } = await import("../src/create-wallet.ts");
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  const { db, storage } = installDb(t);
  const storageKey = newKey();
  const server = { kind: "fixture", label: "offline", tip: async () => 2, blocks: async () => new Uint8Array() };
  const open = (key?: Uint8Array) => createWallet({ network: "regtest", storage, storageKey: key, prewarmProvingKey: false, server });
  const account = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0);

  let wallet = await open(storageKey);
  const restored = await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1 });
  assert.equal(restored.unifiedAddress, account.unifiedAddress);
  await wallet.close();
  const needles = [account.unifiedAddress, account.ufvk, account.transparentAddress!].map(text => new TextEncoder().encode(text));
  for (const [name, value] of db.records) {
    const raw = Buffer.from(serialize(value));
    for (const needle of needles) assert.equal(raw.indexOf(needle), -1, `${String(name)} holds no wallet identifier`);
  }

  const sealed = new Map(db.records);
  for (const key of [undefined, newKey()]) {
    wallet = await open(key);
    await assert.rejects(wallet.load(), code(key ? "store_key_rejected" : "store_locked"));
    await assert.rejects(wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1, replace: true }));
    await wallet.close();
  }
  assert.deepEqual(db.records, sealed, "a locked store cannot be overwritten");
  wallet = await open(storageKey);
  assert.equal((await wallet.load())?.unifiedAddress, account.unifiedAddress);
  await wallet.close();
  await assert.rejects(forgetWallet({ storage }), code("store_locked"));
  await forgetWallet({ storage, storageKey });
  wallet = await open(storageKey);
  assert.equal(await wallet.load(), null);
  await wallet.close();
});

test("an erased store starts again unencrypted or with any key, and inspection stays truthful", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  const { initialize, REGTEST_FAUCET_MNEMONIC, deriveAccount } = await import("../src/lab.ts");
  const { createWallet, forgetWallet } = await import("../src/create-wallet.ts");
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  const { db, storage } = installDb(t);
  const server = { kind: "fixture", label: "offline", tip: async () => 2, blocks: async () => new Uint8Array() };
  const open = (storageKey?: Uint8Array) => createWallet({ network: "regtest", storage, storageKey, prewarmProvingKey: false, server });
  const { unifiedAddress } = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0);
  const restore = async (key?: Uint8Array) => {
    const wallet = await open(key);
    assert.equal((await wallet.restore(REGTEST_FAUCET_MNEMONIC, { birthday: 1, replace: true })).unifiedAddress, unifiedAddress);
    await wallet.close();
  };
  const status = () => inspectWalletStore({ storage });
  const erase = async () => {
    const before = await readWalletGenerationOf(storage);
    await eraseWalletStore({ storage });
    assert.deepEqual(await status(), { exists: false, encrypted: false });
    assert.deepEqual([...db.records.keys()], ["wallet:generation"]);
    assert.notEqual(db.records.get("wallet:generation"), before, "erase fences pages on the old generation");
  };

  const first = newKey(), second = newKey();
  await restore(first);
  assert.deepEqual(await status(), { exists: true, encrypted: true });
  await erase(); // Without the key.
  await restore(second);
  assert.deepEqual(await status(), { exists: true, encrypted: true });
  assert.ok(!("wallet:generation" in Object.fromEntries([...db.records].filter(([, v]) => typeof v === "string"))), "the tombstone is sealed too");
  const reopened = await open(first);
  await assert.rejects(reopened.load(), code("store_key_rejected"));
  await reopened.close();

  await erase();
  await restore();
  assert.deepEqual(await status(), { exists: true, encrypted: false });
  const blocked = await open(newKey());
  await assert.rejects(blocked.load(), code("store_not_encrypted"), "a live unencrypted store is never adopted by a key");
  await blocked.close();

  await erase();
  const third = newKey();
  await restore(third);
  assert.deepEqual(await status(), { exists: true, encrypted: true });
  // Forget leaves a tombstone only; a different key may start again.
  await forgetWallet({ storage, storageKey: third });
  assert.deepEqual(await status(), { exists: false, encrypted: true });
  const plainAgain = await open();
  await assert.rejects(plainAgain.load(), code("store_locked"), "a forgotten encrypted store still needs its key or an erase");
  await plainAgain.close();

  // Store-wide operations refuse to run under an open client.
  const held = await open(third);
  await assert.rejects(eraseWalletStore({ storage }), code("owner_conflict"));
  await held.close();
});

test("an invalid storage key rejects with a typed error and releases the wallet owner", async t => {
  if (!existsSync(gen)) { t.skip("run pnpm build:wasm first"); return; }
  const { initialize } = await import("../src/lab.ts");
  const { createWallet, forgetWallet } = await import("../src/create-wallet.ts");
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  const { storage } = installDb(t);
  const server = { kind: "fixture", label: "offline", tip: async () => 2, blocks: async () => new Uint8Array() };
  const signOnly = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt"]);
  for (const bad of [new Uint8Array(31), signOnly]) {
    await assert.rejects(forgetWallet({ storage, storageKey: bad }), code("store_key_rejected"));
    await assert.rejects(createWallet({ network: "regtest", storage, storageKey: bad, server }), code("store_key_rejected"));
  }
  const key = newKey();
  await forgetWallet({ storage, storageKey: key });
  const wallet = await createWallet({ network: "regtest", storage, storageKey: key, prewarmProvingKey: false, server });
  assert.equal(await wallet.load(), null);
  await wallet.close();
});

function readWalletGenerationOf(storage: WalletStorage): Promise<unknown> {
  return storage.transaction("readonly", tx => tx.get("wallet:generation", value => tx.result(value)));
}
