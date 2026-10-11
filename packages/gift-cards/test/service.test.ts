import assert from "node:assert/strict";
import { test } from "node:test";
import { createGiftCards, giftIdentity, type GiftRecord, type GiftSendOptions, type GiftStore, type GiftTransaction } from "../src/service.ts";
import { encodeGiftLink, GiftError } from "../src/protocol.ts";
const txid = "ab".repeat(32), funding = "cd".repeat(32), fingerprint = "11".repeat(32);
const endpoint = "https://wallet.example/gift";
const card = { network: "regtest" as const, entropy: new Uint8Array(32), birthdayHeight: 10, amountZat: 1_000_000n };
const hasCode = (code: string) => (e: unknown) => e instanceof GiftError && e.code === code;
function fixture(acceptedEndpoints?: readonly string[]) {
  const records = new Map<string, GiftRecord>();
  let fee = 10_000, sends = 0, closes = 0, queries = 0, height = 100, available = 1_010_000, failSave = -1;
  let history: GiftTransaction[] = [{ txid: funding, status: "mined", minedHeight: 90, receivedZat: available, spentZat: 0 }];
  let matureAvailable: number | undefined;
  let exactClaim: GiftTransaction | null | undefined;
  let ackLost = false;
  let failure: object | null = null, claimId = txid, fundingId = funding;
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
    assert.equal(options.minConfirmations, 6, "every actual claim must select six-confirmed inputs");
    assert.equal(options.maxFeeZat, "10000");
    await options.beforeSubmit?.({ txid: claimId }, new AbortController().signal);
    sends++;
    if (failure) throw Object.assign(new Error("synthetic submission outcome"), failure);
    if (ackLost) throw Object.assign(new Error("lost acknowledgement"), { txid: claimId });
    return { txid: claimId };
  };
  const reopen = () => createGiftCards({ network: "regtest", shareEndpoint: endpoint, acceptedEndpoints, store,
    source: { async tip() { queries++; return 100; }, async estimateFee() { queries++; return { feeZat: fee }; },
      async send(_to, amount, options) { assert.equal(options.minConfirmations, undefined); assert.equal(amount, "0.01010000"); await options.beforeSubmit?.({ txid: fundingId }, new AbortController().signal); sends++; if (failure) throw Object.assign(new Error("synthetic submission outcome"), failure); if (ackLost) throw Object.assign(new Error("lost acknowledgement"), { txid: fundingId }); return { txid: fundingId }; } },
    backend: { async create() { return { entropy: card.entropy, address: "giftUA", fingerprint }; },
      async derive() { return { entropy: card.entropy, address: "giftUA", fingerprint }; },
      async open() { return { async getWallet() { throw new Error("unused"); }, async sync() { queries++; return { network: "regtest", unifiedAddress: "giftUA", birthdayHeight: 10, scannedHeight: height, ...(matureAvailable !== undefined ? { matureAvailableZat: matureAvailable } : {}),
        balance: { orchardAvailable: available, saplingAvailable: 0, ironwoodAvailable: 0 } }; }, async history() { return history; },
        ...(exactClaim !== undefined ? { async transaction() { return exactClaim ?? null; } } : {}), async estimateFee() { return { feeZat: fee }; }, send,
        async close() { closes++; } }; }, async dispose() {} },
  });
  const gifts = reopen();
  return { gifts, reopen, records, stats: () => ({ sends, closes, queries }), setFee: (v: number) => { fee = v; },
    setMatureAvailable: (v: number | undefined) => { matureAvailable = v; }, setExactClaim: (v: GiftTransaction | null) => { exactClaim = v; },
    setHeight: (v: number) => { height = v; }, setAvailable: (v: number) => { available = v; }, setHistory: (v: GiftTransaction[]) => { history = v; },
    loseAck: () => { ackLost = true; }, failRevision: (v: number) => { failSave = v; },
    setFailure: (v: object | null) => { failure = v; }, setClaimTxid: (v: string) => { claimId = v; }, setFundingTxid: (v: string) => { fundingId = v; } };
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

test("Vizor endpoint imports require explicit configured trust", async () => {
  const vizor = "https://link.vizor.cash/payment-links/open";
  const link = encodeGiftLink(card, vizor);
  const closed = fixture();
  await assert.rejects(closed.gifts.import(link), hasCode("invalid_link"));
  assert.equal(closed.records.size, 0);
  const allowed = fixture([vizor]);
  assert.equal((await allowed.gifts.import(link)).phase, "draft");
  assert.equal(allowed.stats().queries, 0);
  await assert.rejects(allowed.gifts.import(link.replace("/open#", "/other#")), hasCode("invalid_link"));
});

test("spent mature deposits cannot qualify young replacement notes or pending spend inputs", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  const refill = "ef".repeat(32);
  const spent = { txid, status: "mined", minedHeight: 98, receivedZat: 0, spentZat: 1_010_000 };
  const deposit = { txid: funding, status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 };
  f.setHistory([deposit, spent, { txid: refill, status: "mined", minedHeight: 100, receivedZat: 1_010_000, spentZat: 0 }]);
  assert.equal((await f.gifts.inspect(saved.id)).matureAvailableZat, 0n);
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("confirming"));
  await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("confirming"));
  assert.equal(f.stats().sends, 0);
  f.setHeight(105);
  assert.equal((await f.gifts.prepareRedeem(saved.id, "recipientUA")).amountZat, card.amountZat);
  f.setHistory([deposit, { ...spent, status: "pending", minedHeight: null }, { txid: refill, status: "mined", minedHeight: 105, receivedZat: 1_010_000, spentZat: 0 }]);
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("confirming"));
});

test("self-send and change outputs must mature again; truncated or duplicate history fails closed", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setHistory([
    { txid: funding, status: "mined", minedHeight: 90, receivedZat: 2_020_000, spentZat: 0 },
    { txid, status: "mined", minedHeight: 100, receivedZat: 0, spentZat: 2_020_000, orchardReceived: 1_010_000 },
  ]);
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("confirming"));
  f.setHeight(105);
  assert.equal((await f.gifts.inspect(saved.id)).matureAvailableZat, 1_010_000n);
  assert.equal((await f.gifts.prepareRedeem(saved.id, "recipientUA")).amountZat, card.amountZat);
  f.setHistory(Array.from({ length: 500 }, (_, index) => ({ txid: index.toString(16).padStart(64, "0"), status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 })));
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("confirming"));
  const same = { txid: funding, status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 };
  f.setHistory([same, same]);
  await assert.rejects(f.gifts.inspect(saved.id), hasCode("conflict"));
});

test("a young top-up cannot cover an actual fee beyond the mature amount", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setFee(20_000); f.setAvailable(1_020_000);
  f.setHistory([
    { txid: funding, status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 },
    { txid, status: "mined", minedHeight: 100, receivedZat: 10_000, spentZat: 0 },
  ]);
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("confirming"));
});

test("definitive exact funding rejection retains evidence across restart and requires fresh fee review", async () => {
  const f = fixture(), review = await f.gifts.prepare({ amountZat: card.amountZat });
  f.setAvailable(0); f.setHistory([]);
  f.setFailure({ code: "broadcast_rejected", details: { txid: funding } });
  await assert.rejects(f.gifts.fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  const rejected = f.records.get(review.record.id)!;
  assert.equal(rejected.phase, "funding-rejected");
  assert.deepEqual(rejected.rejectedAttempts, [{ kind: "funding", txid: funding, feeZat: "10000" }]);
  f.setFee(20_000);
  await assert.rejects(f.reopen().fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("review_changed"));
  assert.equal(f.stats().sends, 1);
  f.setFee(10_000); f.setFailure(null); f.setFundingTxid("ef".repeat(32));
  const retried = await f.reopen().fund(review.record.id, { maxFeeZat: 10_000n });
  assert.equal(retried.phase, "funding");
  assert.equal(retried.fundingTxid, "ef".repeat(32));
  assert.deepEqual(retried.rejectedAttempts, rejected.rejectedAttempts);
});

test("definitive redemption rejection retains evidence and permits only a freshly checked retry", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setFailure({ code: "broadcast_rejected", txid });
  await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  const rejected = f.records.get(saved.id)!;
  assert.equal(rejected.phase, "redemption-rejected");
  assert.deepEqual(rejected.rejectedAttempts, [{ kind: "redemption", txid, feeZat: "10000", destination: "recipientUA" }]);
  f.setHeight(94);
  await assert.rejects(f.reopen().prepareRedeem(saved.id, "recipientUA"), hasCode("confirming"));
  f.setHeight(100); f.setFailure(null); f.setClaimTxid("ef".repeat(32));
  await f.reopen().prepareRedeem(saved.id, "recipientUA");
  const retried = await f.reopen().redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n });
  assert.equal(retried.phase, "redeeming");
  assert.equal(retried.claimTxid, "ef".repeat(32));
  assert.deepEqual(retried.rejectedAttempts, rejected.rejectedAttempts);
});

test("unknown, missing, mismatched, or uncommitted rejection evidence cannot authorize retries", async () => {
  for (const failure of [{ code: "broadcast_failed", txid: funding }, { code: "broadcast_rejected" }, { code: "broadcast_rejected", txid }]) {
    const f = fixture(), review = await f.gifts.prepare({ amountZat: card.amountZat });
    f.setFailure(failure);
    await assert.rejects(f.gifts.fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("unresolved"));
    assert.equal(f.records.get(review.record.id)?.phase, "funding");
    assert.equal(f.records.get(review.record.id)?.fundingTxid, funding);
    await assert.rejects(f.reopen().fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("unresolved"));
    assert.equal(f.stats().sends, 1);
  }
  const f = fixture(), review = await f.gifts.prepare({ amountZat: card.amountZat });
  f.setFailure({ code: "broadcast_rejected", txid: funding }); f.failRevision(3);
  await assert.rejects(f.gifts.fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("storage"));
  assert.equal(f.records.get(review.record.id)?.phase, "funding");
  await assert.rejects(f.reopen().fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("unresolved"));
  for (const failure of [{ code: "broadcast_failed", txid }, { code: "broadcast_rejected" }, { code: "broadcast_rejected", txid: funding }]) {
    const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
    f.setFailure(failure);
    await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("unresolved"));
    assert.equal(f.records.get(saved.id)?.phase, "redeeming");
    assert.equal(f.records.get(saved.id)?.claimTxid, txid);
    await assert.rejects(f.reopen().prepareRedeem(saved.id, "recipientUA"), hasCode("unresolved"));
    assert.equal(f.stats().sends, 1);
  }
});

test("positive mined evidence overrides a saved rejection before any retry", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setFailure({ code: "broadcast_rejected", txid });
  await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  f.setHistory([{ txid, status: "mined", minedHeight: 99, receivedZat: 0, spentZat: 1_010_000 }]);
  f.setFailure(null);
  await assert.rejects(f.reopen().redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("unresolved"));
  assert.equal(f.records.get(saved.id)?.phase, "redeemed");
  assert.equal(f.stats().sends, 1);
  const fundingFixture = fixture(), review = await fundingFixture.gifts.prepare({ amountZat: card.amountZat });
  fundingFixture.setAvailable(0); fundingFixture.setHistory([]);
  fundingFixture.setFailure({ code: "broadcast_rejected", txid: funding });
  await assert.rejects(fundingFixture.gifts.fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  fundingFixture.setHistory([{ txid: funding, status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 }]);
  fundingFixture.setFailure(null);
  await assert.rejects(fundingFixture.reopen().fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("unresolved"));
  assert.equal(fundingFixture.records.get(review.record.id)?.phase, "funded");
  assert.equal(fundingFixture.stats().sends, 1);
});

test("authoritative unspent maturity survives bounded history and excludes young replacement notes", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setHistory(Array.from({ length: 500 }, (_, i) => ({ txid: i.toString(16).padStart(64, "0"), status: "mined", minedHeight: 90, receivedZat: 0, spentZat: 0 })));
  f.setMatureAvailable(1_010_000);
  assert.equal((await f.gifts.inspect(saved.id)).matureAvailableZat, 1_010_000n);
  assert.equal((await f.gifts.prepareRedeem(saved.id, "destinationUA")).amountZat, card.amountZat);
  f.setMatureAvailable(0);
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "destinationUA"), hasCode("confirming"));
  f.setMatureAvailable(2_020_000);
  assert.equal((await f.gifts.inspect(saved.id)).matureAvailableZat, 1_010_000n);
  f.setMatureAvailable(-1);
  await assert.rejects(f.gifts.inspect(saved.id), hasCode("conflict"));
});

test("a mined claim outside the recent page stays redeemed until exact lookup proves a reorg", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  await f.gifts.redeem(saved.id, { destination: "destinationUA", maxFeeZat: 10_000n });
  f.setHistory([{ txid, status: "mined", minedHeight: 99, receivedZat: 0, spentZat: 1_010_000 }]);
  assert.equal((await f.gifts.inspect(saved.id)).record.phase, "redeemed");
  f.setHistory(Array.from({ length: 500 }, (_, i) => ({ txid: i.toString(16).padStart(64, "0"), status: "mined", minedHeight: 100, receivedZat: 0, spentZat: 0 })));
  assert.equal((await f.gifts.inspect(saved.id)).record.phase, "redeemed");
  f.setExactClaim({ txid, status: "mined", minedHeight: 99, receivedZat: 0, spentZat: 1_010_000 });
  assert.equal((await f.gifts.inspect(saved.id)).record.phase, "redeemed");
  f.setExactClaim(null);
  assert.equal((await f.gifts.inspect(saved.id)).record.phase, "redeeming");
  await assert.rejects(f.gifts.prepareRedeem(saved.id, "destinationUA"), hasCode("unresolved"));
});

test("truncated history cannot authorize retries of rejected funding or claims without exact lookup", async () => {
  const rows = Array.from({ length: 500 }, (_, i) => ({ txid: i.toString(16).padStart(64, "0"), status: "mined", minedHeight: 100, receivedZat: 0, spentZat: 0 }));
  const issued = fixture();
  issued.setHistory([]); issued.setAvailable(0);
  const review = await issued.gifts.prepare({ amountZat: card.amountZat });
  issued.setFailure({ code: "broadcast_rejected", txid: funding });
  await assert.rejects(issued.gifts.fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  issued.setFailure(null); issued.setHistory(rows);
  await assert.rejects(issued.gifts.fund(review.record.id, { maxFeeZat: 10_000n }), hasCode("unresolved"));
  issued.setExactClaim({ txid: funding, status: "mined", minedHeight: 90, receivedZat: 1_010_000, spentZat: 0 });
  assert.equal((await issued.gifts.inspect(review.record.id)).record.phase, "funded");
  assert.equal(issued.stats().sends, 1);

  const received = fixture(), saved = await received.gifts.import(encodeGiftLink(card, endpoint));
  received.setFailure({ code: "broadcast_rejected", txid });
  await assert.rejects(received.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  received.setFailure(null); received.setHistory(rows); received.setMatureAvailable(1_010_000);
  await assert.rejects(received.gifts.prepareRedeem(saved.id, "recipientUA"), hasCode("unresolved"));
  received.setExactClaim({ txid, status: "mined", minedHeight: 90, receivedZat: 0, spentZat: 1_010_000 });
  assert.equal((await received.gifts.inspect(saved.id)).record.phase, "redeemed");
  assert.equal(received.stats().sends, 1);
});

test("positive later rejected-attempt evidence wins over an earlier truncated absence", async () => {
  const f = fixture(), saved = await f.gifts.import(encodeGiftLink(card, endpoint));
  f.setFailure({ code: "broadcast_rejected", txid });
  await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  const later = "ef".repeat(32);
  f.setClaimTxid(later); f.setFailure({ code: "broadcast_rejected", txid: later });
  await assert.rejects(f.gifts.redeem(saved.id, { destination: "recipientUA", maxFeeZat: 10_000n }), hasCode("broadcast_rejected"));
  const rows = Array.from({ length: 499 }, (_, i) => ({ txid: i.toString(16).padStart(64, "0"), status: "mined", minedHeight: 100, receivedZat: 0, spentZat: 0 }));
  f.setHistory([...rows, { txid: later, status: "mined", minedHeight: 99, receivedZat: 0, spentZat: 1_010_000 }]);
  const inspection = await f.gifts.inspect(saved.id);
  assert.equal(inspection.record.phase, "redeemed");
  assert.equal(inspection.record.claimTxid, later);
  assert.equal(f.stats().sends, 2);
});
