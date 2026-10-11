import assert from "node:assert/strict";
import { test } from "node:test";
import { createGiftCards, giftIdentity, type GiftRecord, type GiftSendOptions, type GiftStore, type GiftTransaction } from "../src/service.ts";
import { encodeGiftLink, GiftError } from "../src/protocol.ts";
const txid = "ab".repeat(32), funding = "cd".repeat(32), fingerprint = "11".repeat(32);
const endpoint = "https://wallet.example/gift";
const card = { network: "regtest" as const, entropy: new Uint8Array(32), birthdayHeight: 10, amountZat: 1_000_000n };
const hasCode = (code: string) => (e: unknown) => e instanceof GiftError && e.code === code;
function fixture() {
  const records = new Map<string, GiftRecord>();
  let fee = 10_000, sends = 0, closes = 0, queries = 0, height = 100, available = 1_010_000, failSave = -1;
  let history: GiftTransaction[] = [{ txid: funding, status: "mined", minedHeight: 90, receivedZat: available, spentZat: 0 }];
  let ackLost = false;
  const store: GiftStore = {
    async get(id) { return records.get(id) ?? null; }, async list() { return [...records.values()]; },
    async save(record, expected) {
      if (record.revision === failSave) throw new GiftError("storage");
      if ((records.get(record.id)?.revision ?? null) !== expected) throw new GiftError("conflict");
      records.set(record.id, structuredClone(record));
    }, async lock(_id, body) { return body(); },
  };
  const send = async (_to: string, amount: string, options: GiftSendOptions) => {
    assert.equal(amount, "0.01000000");
    assert.equal(options.maxFeeZat, "10000");
    await options.beforeSubmit?.({ txid }, new AbortController().signal);
    sends++;
    if (ackLost) throw Object.assign(new Error("lost acknowledgement"), { txid });
    return { txid };
  };
  const gifts = createGiftCards({ network: "regtest", shareEndpoint: endpoint, store,
    source: { async tip() { queries++; return 100; }, async estimateFee() { queries++; return { feeZat: fee }; },
      async send(_to, amount, options) { assert.equal(amount, "0.01010000"); await options.beforeSubmit?.({ txid: funding }, new AbortController().signal); sends++; if (ackLost) throw Object.assign(new Error("lost acknowledgement"), { txid: funding }); return { txid: funding }; } },
    backend: { async create() { return { entropy: card.entropy, address: "giftUA", fingerprint }; },
      async derive() { return { entropy: card.entropy, address: "giftUA", fingerprint }; },
      async open() { return { async getWallet() { throw new Error("unused"); }, async sync() { queries++; return { network: "regtest", unifiedAddress: "giftUA", birthdayHeight: 10, scannedHeight: height,
        balance: { orchardAvailable: available, saplingAvailable: 0, ironwoodAvailable: 0 } }; }, async history() { return history; }, async estimateFee() { return { feeZat: fee }; }, send,
        async close() { closes++; } }; }, async dispose() {} },
  });
  return { gifts, records, stats: () => ({ sends, closes, queries }), setFee: (v: number) => { fee = v; },
    setHeight: (v: number) => { height = v; }, setAvailable: (v: number) => { available = v; }, setHistory: (v: GiftTransaction[]) => { history = v; },
    loseAck: () => { ackLost = true; }, failRevision: (v: number) => { failSave = v; } };
}
test("import is offline and metadata cannot select another account namespace", async () => {
  const f = fixture(), link = encodeGiftLink(card, endpoint), saved = await f.gifts.import(link);
  assert.equal(saved.id, giftIdentity(card.network, fingerprint));
  assert.equal(f.stats().queries, 0);
  for (const changed of [{ birthdayHeight: 11 }, { amountZat: 2_000_000n }, { label: "changed" }])
    await assert.rejects(f.gifts.import(encodeGiftLink({ ...card, ...changed }, endpoint)), hasCode("conflict"));
  assert.equal((await f.gifts.list()).length, 1);
});
test("funding draft commits before quotation and final cap rejects fee changes", async () => {
  const f = fixture(), review = await f.gifts.prepare({ amountZat: card.amountZat });
  assert.equal(f.records.get(review.record.id)?.phase, "draft");
  f.setFee(20_000);
  await assert.rejects(f.gifts.fund(review.record.id, { maxFeeZat: review.feeZat }), hasCode("review_changed"));
  assert.equal(f.stats().sends, 0);
  f.setFee(10_000); f.loseAck();
  await assert.rejects(f.gifts.fund(review.record.id, { maxFeeZat: review.feeZat }), hasCode("unresolved"));
  assert.equal(f.records.get(review.record.id)?.fundingTxid, funding);
  await assert.rejects(f.gifts.fund(review.record.id, { maxFeeZat: review.feeZat }), hasCode("unresolved"));
  assert.equal(f.stats().sends, 1);
});
test("redemption keeps exact face value, durable txid, maturity and fee boundaries", async () => {
  const f = fixture(), record = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setHeight(94);
  await assert.rejects(f.gifts.prepareRedeem(record.id, "recipientUA"), hasCode("confirming"));
  f.setHeight(95); f.setAvailable(1_000_000);
  await assert.rejects(f.gifts.prepareRedeem(record.id, "recipientUA"), hasCode("insufficient_funds"));
  f.setAvailable(2_010_000);
  const review = await f.gifts.prepareRedeem(record.id, "recipientUA");
  assert.equal(review.amountZat, card.amountZat);
  f.loseAck();
  await assert.rejects(f.gifts.redeem(record.id, { destination: review.destination, maxFeeZat: review.feeZat }), hasCode("unresolved"));
  const saved = f.records.get(record.id)!;
  assert.equal(saved.phase, "redeeming"); assert.equal(saved.claimTxid, txid);
  await assert.rejects(f.gifts.redeem(record.id, { destination: "otherUA", maxFeeZat: review.feeZat }), hasCode("unresolved"));
  assert.equal(f.stats().sends, 1); assert.ok(f.stats().closes >= 5);
});
test("journal quota failure before submission sends no bytes", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.failRevision(3); // funded r1, intent r2, transaction correlation r3
  await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("unresolved"));
  assert.equal(f.stats().sends, 0);
  assert.equal(f.records.get(saved.id)?.phase, "redeeming");
});
test("inspection needs positive mined evidence and downgrades reorged redemption", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setHistory([]);
  assert.equal((await f.gifts.inspect(saved.id)).confirmations, 0);
  f.setHistory([{ txid: funding, status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 }]);
  await f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n });
  f.setHistory([{ txid, status: "mined", minedHeight: 99, receivedZat: 0, spentZat: 1_010_000 }]);
  assert.equal((await f.gifts.inspect(saved.id)).record.phase, "redeemed");
  f.setHistory([]);
  assert.equal((await f.gifts.inspect(saved.id)).record.phase, "redeeming");
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("unresolved"));
});
