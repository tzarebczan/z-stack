import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { setImmediate as turn } from "node:timers/promises";
import {
  forgetPasskeySeed,
  hasPasskeySeed,
  PASSKEY_NO_SEED_BLOB,
  PASSKEY_VAULT_KEY,
  passkeyLocalhostUrl,
  passkeyRpId,
  registerPasskeySeed,
  unlockPasskeySeed,
} from "../src/passkey.ts";
import { forgetWasmWallet, peekWasmWallet } from "../src/wasm-client.ts";
import { hasEncryptedSeed, persistEncryptedSeed, unlockEncryptedSeed, forgetEncryptedSeed } from "../src/seed-vault.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";
import { clearSavedWallet, advanceWalletGeneration, readWalletGeneration } from "../src/snapshot-storage.ts";
import { beginWalletOperation, runWalletForget } from "../src/wallet-lifecycle.ts";
import { memoryWalletStorage } from "../src/storage.ts";
import { WalletError } from "@z-stack/core";
import { WALLET_GENERATION_KEY, useWalletStorage } from "../src/wallet-storage.ts";

function installFakeIndexedDB() {
  const fake = memoryIndexedDb();
  const previous = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  const previousRange = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "indexedDB", { value: fake.indexedDB, configurable: true });
  Object.defineProperty(globalThis, "IDBKeyRange", { value: fake.IDBKeyRange, configurable: true });
  return () => {
    if (previous) Object.defineProperty(globalThis, "indexedDB", previous);
    else Reflect.deleteProperty(globalThis, "indexedDB");
    if (previousRange) Object.defineProperty(globalThis, "IDBKeyRange", previousRange);
    else Reflect.deleteProperty(globalThis, "IDBKeyRange");
  };
}

async function idbPut(key: IDBValidKey, value: unknown) {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const req = indexedDB.open("z-stack-wasm", 1);
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains("wallets")) req.result.createObjectStore("wallets");
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction("wallets", "readwrite");
    tx.objectStore("wallets").put(value, key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

async function idbHas(key: IDBValidKey) {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const req = indexedDB.open("z-stack-wasm", 1);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  return new Promise<boolean>((resolve, reject) => {
    const tx = db.transaction("wallets", "readonly");
    const r = tx.objectStore("wallets").get(key);
    r.onsuccess = () => resolve(r.result !== undefined);
    r.onerror = () => reject(r.error);
  });
}

test("passkeyRpId accepts localhost and rejects IPv4", () => {
  assert.equal(passkeyRpId("localhost"), "localhost");
  assert.equal(passkeyRpId("foo.localhost"), "localhost");
  assert.equal(passkeyRpId("127.0.0.1"), null);
  assert.equal(passkeyRpId("::1"), null);
  assert.equal(passkeyRpId("[::1]"), null);
  assert.equal(passkeyRpId("wallet.example"), "wallet.example");
});

test("passkeyLocalhostUrl is unset without a page", () => {
  assert.equal(passkeyLocalhostUrl(), undefined);
});

test("forgetWasmWallet keeps passkey.v1 and drops snapshot + seed.enc", async () => {
  const restore = installFakeIndexedDB();
  try {
    await idbPut("default", new Uint8Array([1, 2, 3]));
    await idbPut("meta", { unlockPolicy: "session" });
    await idbPut("seed.enc", { v: 2, salt: "x", iv: "y", ct: "z" });
    await idbPut(PASSKEY_VAULT_KEY, { passkeys: [{ credentialId: "abc" }] });
    assert.equal(await hasPasskeySeed(), true);
    assert.equal(await hasEncryptedSeed(), true);

    await forgetWasmWallet();

    assert.equal(await hasPasskeySeed(), true);
    assert.equal(await idbHas(PASSKEY_VAULT_KEY), true);
    assert.equal(await hasEncryptedSeed(), false);
    assert.equal(await idbHas("default"), false);
    assert.equal(await idbHas("meta"), false);
    assert.equal(await peekWasmWallet(), null);

    await forgetWasmWallet({ passkey: true });
    assert.equal(await hasPasskeySeed(), false);
    assert.equal(await idbHas(PASSKEY_VAULT_KEY), false);
  } finally {
    restore();
  }
});

test("forgetPasskeySeed drops only the passkey record", async () => {
  const restore = installFakeIndexedDB();
  try {
    await idbPut("default", new Uint8Array([9]));
    await idbPut("passkey.v1", { credId: "xyz", mode: "largeBlob", createdAt: 1 });
    await forgetPasskeySeed();
    assert.equal(await hasPasskeySeed(), false);
    assert.equal(await idbHas("default"), true);
  } finally {
    restore();
  }
});

test("missing blob copy is honest about PRF-only recovery", () => {
  assert.match(PASSKEY_NO_SEED_BLOB, /no seed blob/);
  assert.match(PASSKEY_NO_SEED_BLOB, /PRF copy/);
});

function fixture(t: TestContext) {
  const db = memoryIndexedDb();
  for (const [key, value] of Object.entries({ indexedDB: db.indexedDB, IDBKeyRange: db.IDBKeyRange })) {
    globalValue(t, key, value);
  }
  return db;
}

function globalValue(t: TestContext, key: string, value: unknown) {
  const previous = Object.getOwnPropertyDescriptor(globalThis, key);
  Object.defineProperty(globalThis, key, { value, configurable: true });
  t.after(() => {
    if (previous) Object.defineProperty(globalThis, key, previous);
    else Reflect.deleteProperty(globalThis, key);
  });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(r => { resolve = r; });
  return { promise, resolve };
}

function fastCrypto(t: TestContext) {
  return {
    importKey: t.mock.method(crypto.subtle, "importKey", async () => ({} as CryptoKey)),
    deriveKey: t.mock.method(crypto.subtle, "deriveKey", async () => ({} as CryptoKey)),
    encrypt: t.mock.method(crypto.subtle, "encrypt", async () => new Uint8Array([7, 8, 9]).buffer),
  };
}

function credential(ext: Record<string, unknown> = {}) {
  return {
    type: "public-key", rawId: new Uint8Array([1, 2, 3]).buffer,
    response: { getTransports: () => ["internal"] }, getClientExtensionResults: () => ext,
  } as unknown as PublicKeyCredential;
}

function webauthn(t: TestContext, methods: { create: (opts: CredentialCreationOptions) => Promise<unknown>; get: (opts: CredentialRequestOptions) => Promise<unknown> }) {
  // Not localhost: portable copies (largeBlob) are off there, since every
  // local port shares the RP ID.
  globalValue(t, "location", { hostname: "wallet.example", host: "wallet.example", protocol: "https:" });
  globalValue(t, "PublicKeyCredential", class {});
  globalValue(t, "navigator", { credentials: methods });
}

test("encrypted seed roundtrip closes handles and preserves wrong-passphrase behavior", async (t) => {
  const db = fixture(t);
  await persistEncryptedSeed("test passphrase", "public test fixture words");
  assert.equal(await hasEncryptedSeed(), true);
  assert.equal(await unlockEncryptedSeed("test passphrase"), "public test fixture words");
  await assert.rejects(unlockEncryptedSeed("wrong"), /wrong passphrase/);
  assert.equal(db.closes, db.openCount);
});

test("forget cancels encryption at derivation and encryption without seed resurrection", async (t) => {
  for (const stage of ["deriveKey", "encrypt"] as const) {
    await t.test(stage, async t => {
      const db = fixture(t);
      const cryptoMock = fastCrypto(t);
      const pending = deferred<any>();
      const entered = deferred<void>();
      cryptoMock[stage].mock.mockImplementation(() => { entered.resolve(); return pending.promise; });
      const saving = persistEncryptedSeed("test", "public fixture");
      const rejected = assert.rejects(saving, error => (error as Error).name === "AbortError");
      await entered.promise;
      await forgetWasmWallet();
      await rejected; // Cancellation must not wait for uninterruptible crypto.
      pending.resolve(stage === "encrypt" ? new Uint8Array([1]).buffer : {});
      await turn();
      assert.equal(db.records.has("seed.enc"), false);
      assert.deepEqual([...db.records.keys()], [WALLET_GENERATION_KEY]);
      assert.equal(db.closes, db.openCount);
    });
  }
});

test("cancelled vault opens and active writes close handles and settle on abort", async (t) => {
  const db = fixture(t);
  fastCrypto(t);
  const controller = new AbortController();
  db.holdOpens = true;
  const saving = persistEncryptedSeed("test", "fixture", controller.signal);
  const rejected = assert.rejects(saving, error => (error as Error).name === "AbortError");
  await turn();
  assert.equal(db.opens.length, 1);
  controller.abort();
  await rejected;
  db.opens.shift()!();
  db.holdOpens = false;
  assert.equal(db.closes, db.openCount, "a late successful open must close immediately");

  db.holdCommits = true;
  const inTransaction = persistEncryptedSeed("test", "fixture");
  const aborted = assert.rejects(inTransaction, error => (error as Error).name === "AbortError");
  await turn();
  assert.equal(db.commits.length, 1);
  db.holdCommits = false;
  await forgetEncryptedSeed();
  await aborted;
  db.commits.shift()!();
  assert.equal(db.records.has("seed.enc"), false);
  assert.equal(db.closes, db.openCount);
});

test("an old enclosing signal cannot start vault work or adopt a fresh durable generation", async (t) => {
  const db = fixture(t);
  const old = beginWalletOperation();
  beginWalletOperation();
  await assert.rejects(persistEncryptedSeed("test", "fixture", old.signal), error => (error as Error).name === "AbortError");
  await assert.rejects(registerPasskeySeed("fixture", old.signal), error => (error as Error).name === "AbortError");
  assert.equal(db.openCount, 0);
});

test("a different tab's wallet-generation change rejects a pending encrypted seed write", async (t) => {
  const db = fixture(t);
  const cryptoMock = fastCrypto(t);
  const encrypted = deferred<ArrayBuffer>();
  const entered = deferred<void>();
  cryptoMock.encrypt.mock.mockImplementation(() => { entered.resolve(); return encrypted.promise; });
  const saving = persistEncryptedSeed("test", "old wallet");
  const rejected = assert.rejects(saving, /saved wallet changed/);
  await entered.promise;
  await turn();
  // No local lifecycle invalidation: represents another JS context changing IDB.
  await advanceWalletGeneration();
  encrypted.resolve(new Uint8Array([1]).buffer);
  await rejected;
  assert.equal(db.records.has("seed.enc"), false);
});

test("forget during passkey create aborts the chooser and prevents later assertions or writes", async (t) => {
  const db = fixture(t);
  db.records.set("passkey.v1", { credId: "previous", mode: "prf", ct: "keep" });
  const before = structuredClone(db.records.get("passkey.v1"));
  const created = deferred<PublicKeyCredential>();
  let signal: AbortSignal | undefined;
  let assertions = 0;
  webauthn(t, {
    create: options => { signal = options.signal; return created.promise; },
    get: async () => { assertions++; return credential(); },
  });
  const registering = registerPasskeySeed("public fixture");
  const rejected = assert.rejects(registering, error => (error as Error).name === "AbortError");
  assert.ok(signal, "create must run immediately from the initiating gesture");
  await forgetWasmWallet();
  await rejected;
  assert.equal(signal.aborted, true);
  created.resolve(credential());
  await turn();
  assert.equal(assertions, 0);
  assert.deepEqual(db.records.get("passkey.v1"), before);
});

test("standalone passkey deletion cancels assertion while leaving an independent seed write live", async (t) => {
  const db = fixture(t);
  fastCrypto(t);
  const assertion = deferred<PublicKeyCredential>();
  const entered = deferred<void>();
  let signal: AbortSignal | undefined;
  webauthn(t, {
    create: async () => credential(),
    get: options => { signal = options.signal; entered.resolve(); return assertion.promise; },
  });
  const registering = registerPasskeySeed("public fixture");
  const rejected = assert.rejects(registering, error => (error as Error).name === "AbortError");
  await entered.promise;
  const seed = persistEncryptedSeed("test", "independent fixture");
  await forgetPasskeySeed();
  await rejected;
  assertion.resolve(credential({ largeBlob: { written: true } }));
  await seed;
  assert.equal(signal?.aborted, true);
  assert.equal(db.records.has("passkey.v1"), false);
  assert.equal(db.records.has("seed.enc"), true);
  assert.equal(db.closes, db.openCount);
});

test("passkey PRF wrapping cancelled by explicit forget cannot republish the local record", async (t) => {
  const db = fixture(t);
  const cryptoMock = fastCrypto(t);
  const encryption = deferred<ArrayBuffer>();
  const entered = deferred<void>();
  cryptoMock.encrypt.mock.mockImplementation(() => { entered.resolve(); return encryption.promise; });
  webauthn(t, {
    create: async () => credential(),
    get: async () => credential({ prf: { results: { first: new Uint8Array(32).buffer } } }),
  });
  const registering = registerPasskeySeed("public fixture");
  const rejected = assert.rejects(registering, error => (error as Error).name === "AbortError");
  await entered.promise;
  await forgetWasmWallet({ passkey: true });
  await rejected;
  encryption.resolve(new Uint8Array([9]).buffer);
  await turn();
  assert.equal(db.records.has("passkey.v1"), false);
  assert.equal(db.records.has(PASSKEY_VAULT_KEY), false);
});

test("late passkey unlock cannot return a mnemonic after forget and does not retry fallback", async (t) => {
  const db = fixture(t);
  const assertion = deferred<PublicKeyCredential>();
  const entered = deferred<void>();
  let gets = 0;
  webauthn(t, {
    create: async () => credential(),
    get: async () => { gets++; entered.resolve(); return assertion.promise; },
  });
  const unlocking = unlockPasskeySeed();
  const rejected = assert.rejects(unlocking, error => (error as Error).name === "AbortError");
  await entered.promise;
  await forgetWasmWallet();
  await rejected;
  assertion.resolve(credential({ largeBlob: { blob: new TextEncoder().encode("obsolete blob fixture").buffer } }));
  await turn();
  assert.equal(gets, 1);
  assert.equal(db.records.has("passkey.v1"), false);
});

test("WebAuthn does not queue a chooser behind forget cleanup", async (t) => {
  fixture(t);
  let creates = 0;
  webauthn(t, { create: async () => { creates++; return credential(); }, get: async () => credential() });
  const cleanup = deferred<void>();
  const forgetting = runWalletForget(() => cleanup.promise);
  await assert.rejects(registerPasskeySeed("public fixture"), /forget is still in progress/);
  cleanup.resolve();
  await forgetting;
  assert.equal(creates, 0);
});

test("passkey registration and unlock round-trip through the PRF vault", async (t) => {
  const db = fixture(t);
  const words = "public passkey test fixture";
  const secret = new Uint8Array(32).fill(8).buffer;
  webauthn(t, {
    create: async () => credential(),
    get: async () => credential({ prf: { results: { first: secret } } }),
  });
  assert.equal((await registerPasskeySeed(words)).mode, "prf");
  assert.equal(db.records.has(PASSKEY_VAULT_KEY), true);
  assert.ok(!JSON.stringify(db.records.get(PASSKEY_VAULT_KEY)).includes("fixture"), "the record holds ciphertext only");
  assert.equal(await unlockPasskeySeed(), words);
  assert.equal(db.closes, db.openCount);
});

test("an authenticator without PRF is refused instead of storing the seed in the clear", async (t) => {
  const db = fixture(t);
  webauthn(t, {
    create: async () => credential({ prf: { enabled: false } }),
    get: async () => credential(),
  });
  await assert.rejects(registerPasskeySeed("public fixture"), (e) => (e as { code?: string }).code === "prf-unsupported");
  assert.equal(db.records.has(PASSKEY_VAULT_KEY), false);
});

test("old passkey backups require phrase recovery and never start a legacy ceremony", async (t) => {
  const db = fixture(t);
  const words = "public legacy fixture words";
  db.records.set("passkey.v1", { credId: "AQID", userId: "AA", mode: "largeBlob", createdAt: 1 });
  webauthn(t, {
    create: async () => credential(),
    get: async () => credential({ largeBlob: { blob: new TextEncoder().encode(words).buffer } }),
  });
  assert.equal(await hasPasskeySeed(), false);
  await assert.rejects(unlockPasskeySeed(), (error: unknown) =>
    error instanceof Error && "code" in error && error.code === "no-vault" &&
    String(error.cause).includes("Old passkey backups are not supported"));
});

test("forget during passphrase decryption rejects instead of returning stale words or wrong-passphrase", async (t) => {
  const db = fixture(t);
  fastCrypto(t);
  await persistEncryptedSeed("test", "public fixture");
  const plaintext = deferred<ArrayBuffer>();
  const entered = deferred<void>();
  t.mock.method(crypto.subtle, "decrypt", () => { entered.resolve(); return plaintext.promise; });
  const unlocking = unlockEncryptedSeed("test");
  const rejected = assert.rejects(unlocking, error => (error as Error).name === "AbortError");
  await entered.promise;
  await forgetWasmWallet();
  await rejected;
  plaintext.resolve(new TextEncoder().encode("public fixture").buffer);
  await turn();
  assert.equal(db.records.has("seed.enc"), false);
  assert.equal(db.closes, db.openCount);
});

test("vault writes cannot adopt a newer generation than their enclosing wallet operation", async (t) => {
  const db = fixture(t);
  fastCrypto(t);
  const expected = await readWalletGeneration();
  await advanceWalletGeneration();
  // Another document advanced IDB; this document's captured signal is still live.
  const caller = new AbortController();
  await assert.rejects(persistEncryptedSeed("test", "old words", caller.signal, expected), /saved wallet changed/);
  assert.equal(caller.signal.aborted, false);
  assert.equal(db.records.has("seed.enc"), false);

  const baseline = deferred<string>();
  const created = deferred<PublicKeyCredential>();
  let chooserSignal: AbortSignal | undefined;
  let gets = 0;
  webauthn(t, {
    create: options => { chooserSignal = options.signal; return created.promise; },
    get: async () => { gets++; return credential(); },
  });
  const registering = registerPasskeySeed("old words", caller.signal, baseline.promise);
  const rejected = assert.rejects(registering, /saved wallet changed/);
  assert.ok(chooserSignal, "capturing a baseline Promise must not defer the initiating gesture");
  baseline.resolve(expected);
  await rejected;
  assert.equal(chooserSignal.aborted, true);
  created.resolve(credential());
  await turn();
  assert.equal(gets, 0);
  assert.equal(db.records.has("passkey.v1"), false);
  assert.equal(db.records.has(PASSKEY_VAULT_KEY), false);
});


for (const [name, forget, key] of [
  ["encrypted seed", forgetEncryptedSeed, "seed.enc"],
  ["passkey", forgetPasskeySeed, PASSKEY_VAULT_KEY],
] as const) {
  test(`${name} deletion rejects unavailable storage, preserves its record and can retry`, async t => {
    const backing = memoryWalletStorage();
    let available = true;
    useWalletStorage({ get available() { return available; }, transaction: (...args) => backing.transaction(...args) });
    t.after(() => useWalletStorage());
    const record = { encrypted: "public-ciphertext-fixture" };
    await backing.transaction("readwrite", tx => { tx.put(key, record); tx.put("default", { wallet: "unchanged" }); });
    available = false;
    await assert.rejects(forget(), error => error instanceof WalletError && error.code === "wallet_db");
    assert.deepEqual(await backing.transaction("readonly", tx => tx.get(key, value => tx.result(value))), record);
    available = true; await forget();
    assert.equal(await backing.transaction("readonly", tx => tx.get(key, value => tx.result(value))), undefined);
    assert.deepEqual(await backing.transaction("readonly", tx => tx.get("default", value => tx.result(value))), { wallet: "unchanged" });
  });
}
