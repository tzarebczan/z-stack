import assert from "node:assert/strict";
import { test } from "node:test";
import { IDBFactory } from "fake-indexeddb";
import { createEncryptedGiftStore, parseGiftRecord } from "../src/store.ts";
import { GiftError } from "../src/protocol.ts";
import type { GiftRecord } from "../src/service.ts";
const code = (v: string) => (e: unknown) => e instanceof GiftError && e.code === v;
const key = () => crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
const draft: GiftRecord = { id: "regtest:" + "aa".repeat(32), revision: 0, role: "issued", phase: "draft", address: "giftUA", link: "https://wallet.example/gift#v3=SECRET_BEARER_ENTROPY", amountZat: "1000000" };
async function raw(factory: IDBFactory, body: (store: IDBObjectStore) => IDBRequest) {
  const db = await new Promise<IDBDatabase>((resolve, reject) => { const r = factory.open("gifts", 1); r.onsuccess = () => resolve(r.result); r.onerror = reject; });
  try {
    return await new Promise<unknown>((resolve, reject) => { const tx = db.transaction("gifts", "readwrite"); const r = body(tx.objectStore("gifts")); tx.oncomplete = () => resolve(r.result); tx.onabort = tx.onerror = reject; });
  } finally { db.close(); }
}
test("encrypted journal commits before returning and rejects stale concurrent revisions", async () => {
  const factory = new IDBFactory(), aes = await key(), opts = { name: "gifts", key: aes, factory };
  const first = createEncryptedGiftStore(opts), second = createEncryptedGiftStore(opts);
  await first.save(draft, null);
  assert.deepEqual(await second.get(draft.id), draft);
  assert.deepEqual(await first.list(), [draft]);
  const serialized = JSON.stringify(await raw(factory, s => s.get(draft.id)));
  assert.ok(!serialized.includes("SECRET_BEARER_ENTROPY") && !serialized.includes("giftUA"));
  const next = { ...draft, phase: "funding" as const, revision: 1 };
  const results = await Promise.allSettled([first.save(next, 0), second.save(next, 0)]);
  assert.equal(results.filter(r => r.status === "fulfilled").length, 1);
  assert.equal(results.filter(r => r.status === "rejected").length, 1);
  assert.equal((await second.get(draft.id))?.revision, 1);
  await assert.rejects(second.save(next, 0), code("conflict"));
});
test("wrong keys, tampered revisions and transplanted ciphertext never reveal secrets", async () => {
  const factory = new IDBFactory(), aes = await key(), store = createEncryptedGiftStore({ name: "gifts", key: aes, factory });
  await store.save(draft, null);
  await assert.rejects(createEncryptedGiftStore({ name: "gifts", key: await key(), factory }).get(draft.id), code("storage"));
  const envelope = await raw(factory, s => s.get(draft.id));
  assert.ok(typeof envelope === "object" && envelope !== null);
  await raw(factory, s => s.put(envelope, "regtest:" + "bb".repeat(32)));
  await assert.rejects(store.get("regtest:" + "bb".repeat(32)), code("storage"));
  await raw(factory, s => s.put({ ...envelope, revision: 3 }, draft.id));
  await assert.rejects(store.get(draft.id), code("storage"));
});

test("incompatible persisted phases cannot discard prior payment evidence", () => {
  for (const raw of [{ ...draft, fundingTxid: "aa".repeat(32) }, { ...draft, claimTxid: "aa".repeat(32) },
    { ...draft, destination: "recipient" }, { ...draft, feeZat: "10000" },
    { ...draft, phase: "funding", claimTxid: "aa".repeat(32) }, { ...draft, phase: "funded", destination: "recipient" }])
    assert.throws(() => parseGiftRecord(raw), code("storage"));
});

test("definitive rejection phases persist exact attempt evidence and reject inconsistent retry state", async () => {
  const factory = new IDBFactory(), store = createEncryptedGiftStore({ name: "gifts", key: await key(), factory });
  const fundingTxid = "ab".repeat(32), claimTxid = "cd".repeat(32);
  const funding: GiftRecord = { ...draft, phase: "funding-rejected", fundingTxid, feeZat: "10000",
    rejectedAttempts: [{ kind: "funding", txid: fundingTxid, feeZat: "10000" }] };
  await store.save(funding, null);
  assert.deepEqual(await store.get(draft.id), funding);
  const redemption: GiftRecord = { ...funding, revision: 1, phase: "redemption-rejected", claimTxid, destination: "recipientUA",
    rejectedAttempts: [...(funding.rejectedAttempts ?? []), { kind: "redemption", txid: claimTxid, feeZat: "10000", destination: "recipientUA" }] };
  await store.save(redemption, 0);
  assert.deepEqual(await store.get(draft.id), redemption);
  for (const raw of [{ ...funding, rejectedAttempts: [] }, { ...funding, role: "received" },
    { ...funding, feeZat: "20000" }, { ...funding, fundingTxid: claimTxid },
    { ...redemption, destination: "otherUA" }, { ...redemption, claimTxid: fundingTxid },
    { ...redemption, rejectedAttempts: [{ kind: "unknown", txid: claimTxid, feeZat: "10000" }] },
    { ...redemption, rejectedAttempts: [{ kind: "redemption", txid: "invalid", feeZat: "10000", destination: "recipientUA" }] },
    { ...funding, rejectedAttempts: Array.from({ length: 257 }, () => funding.rejectedAttempts?.[0]) }])
    assert.throws(() => parseGiftRecord(raw), code("storage"));
});
