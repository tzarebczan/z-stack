import assert from "node:assert/strict";
import { test } from "node:test";
import { memoryVaultStore } from "@z-stack/passkey";
import { indexedDbWalletStorage, memoryWalletStorage } from "../src/storage.ts";
import { checkWalletStorageAdapter, checkVaultStoreAdapter, AdapterConformanceError } from "../src/lab/adapter-conformance.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";

test("published conformance helpers accept independent connections to the reference stores", async t => {
  const memory = memoryWalletStorage();
  const wallet = await checkWalletStorageAdapter({ open: () => memory, dispose() {} });
  assert.equal(wallet.passed.length, 6); assert.ok(wallet.untested.some(value => value.includes("crash")));
  const idb = memoryIndexedDb();
  const old = Object.getOwnPropertyDescriptor(globalThis, "IDBKeyRange");
  Object.defineProperty(globalThis, "IDBKeyRange", { configurable: true, value: idb.IDBKeyRange });
  t.after(() => old ? Object.defineProperty(globalThis, "IDBKeyRange", old) : Reflect.deleteProperty(globalThis, "IDBKeyRange"));
  const indexed = await checkWalletStorageAdapter({ open: () => indexedDbWalletStorage({ name: "conformance-disposable", factory: idb.indexedDB }), dispose() {} });
  assert.equal(indexed.passed.length, 6);
  const vault = memoryVaultStore();
  const result = await checkVaultStoreAdapter({ open: () => vault, dispose() {} });
  assert.equal(result.passed.length, 5);
});
test("conformance rejects independent per-connection storage and still disposes its fixture", async () => {
  let disposed = false;
  await assert.rejects(checkWalletStorageAdapter({ open: () => memoryWalletStorage(), dispose() { disposed = true; } }),
    error => error instanceof AdapterConformanceError);
  assert.equal(disposed, true);
});
test("commit-failure hooks verify rollback instead of silently omitting quota coverage", async () => {
  const store = memoryWalletStorage();
  let fail = false;
  const wrapped = { ...store, transaction: (...args: Parameters<typeof store.transaction>) => {
    if (fail && args[0] === "readwrite") { fail = false; return Promise.reject(new DOMException("fixture quota", "QuotaExceededError")); }
    return store.transaction(...args);
  } };
  const result = await checkWalletStorageAdapter({ open: () => wrapped, dispose() {}, failNextCommit() { fail = true; } });
  assert.equal(result.passed.length, 7);
  assert.ok(!result.untested.some(value => value.includes("quota")));
});

test("unexpected adapter and cleanup failures do not expose provider payloads", async () => {
  for (const check of [checkWalletStorageAdapter, checkVaultStoreAdapter]) {
    await assert.rejects(check({ open: async () => { throw new Error("private viewing key / provider URL"); }, dispose() {} } as never),
      (error: unknown) => error instanceof AdapterConformanceError && error.check === "adapter-operation" && !error.message.includes("private"));
    await assert.rejects(check({ open: async () => { throw new Error("private wallet"); }, dispose() { throw new Error("private storage path"); } } as never),
      (error: unknown) => error instanceof AdapterConformanceError && error.check === "fixture-dispose" && !error.message.includes("private"));
  }
});
