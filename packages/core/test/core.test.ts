import assert from "node:assert/strict";
import { test } from "node:test";
import {
  canSend,
  canSendReason,
  canShield,
  filterHistory,
  classifyHistory,
  classifyHistoryList,
  DATE_SAFETY_BLOCKS,
  formatZatoshis,
  heightFromDate,
  inferTransactionType,
  parseBirthdayInput,
  parseZecToZatoshis,
  zecToZatoshis,
  parseZip321,
  catchUpPercent,
  displayCatchUpPercent,
  historicOverlayVisible,
  prefetchBuffer,
  syncTuning,
  grpcWebPrefetch,
  pipePrefetch,
  lightStallWarning,
  liveScanEtaSecs,
  QUIET_BEHIND_BLOCKS,
  STALL_QUIET_REMAINING,
  zip321Uri,
  zip321UriMany,
  WalletError,
  classifyWalletError,
  walletErrorMessage,
  classifyUaReceiverSet,
  inspectAddressSummary,
  parseUaReceiverSet,
  uaReceiverSetLabel,
  uaIncludesTransparent,
  uaReceivers,
  DEFAULT_UA_RECEIVER_SET,
  type HistoryEntry,
} from "../src/index.ts";

test("formatZatoshis", () => {
  assert.equal(formatZatoshis(0n), "0.00000000");
  assert.equal(formatZatoshis(1n), "0.00000001");
  assert.equal(formatZatoshis(50_000n), "0.00050000");
  assert.equal(formatZatoshis(100_000_000n), "1.00000000");
  assert.equal(formatZatoshis(68_750_000_000n), "687.50000000");
});

test("browser quota errors give storage recovery advice without suggesting a wallet wipe", () => {
  const cause = new DOMException("The quota has been exceeded.", "QuotaExceededError");
  const error = WalletError.fromUnknown(cause);
  assert.equal(error.code, "storage_full");
  assert.equal(error.cause, cause);
  assert.match(error.userMessage(), /Free device space and retry/);
  assert.doesNotMatch(error.userMessage(), /wipe|clear.*data/);
});

test("unknown provider errors never expose diagnostic payloads through display copy", () => {
  const cause = new Error("SYNTHETIC_PRIVATE_PROVIDER_CONTEXT");
  const error = WalletError.fromUnknown(cause);
  assert.equal(error.code, "unknown");
  assert.doesNotMatch(error.userMessage(), /SYNTHETIC_PRIVATE_PROVIDER_CONTEXT/);
  assert.ok(error.userMessage().length > 0);
  assert.equal(error.cause, cause, "private debugging must retain the original failure");
});

test("parseZecToZatoshis matches engine vectors", () => {
  assert.equal(zecToZatoshis, parseZecToZatoshis);
  assert.equal(parseZecToZatoshis("1"), 100_000_000n);
  assert.equal(parseZecToZatoshis("0.0005"), 50_000n);
  assert.equal(parseZecToZatoshis(".5"), 50_000_000n);
  assert.equal(parseZecToZatoshis("687.5"), 68_750_000_000n);
  assert.throws(() => parseZecToZatoshis(""));
  assert.throws(() => parseZecToZatoshis("abc"));
  assert.throws(() => parseZecToZatoshis("1.123456789"));
  assert.throws(() => parseZecToZatoshis("-1"));
});

test("zip321 roundtrip", () => {
  assert.equal(zip321Uri("uregtest1abc"), "zcash:uregtest1abc");
  assert.equal(zip321Uri("uregtest1abc", "0.0005"), "zcash:uregtest1abc?amount=0.0005");
  const p = parseZip321("zcash:uregtest1abc?amount=0.0005");
  assert.equal(p.address, "uregtest1abc");
  assert.equal(p.amountZat, 50_000n);
  const withMemo = zip321Uri("uregtest1abc", "1", { memo: "hi", label: "pay" });
  const p2 = parseZip321(withMemo);
  assert.equal(p2.memo, "hi");
  assert.equal(p2.label, "pay");
  assert.throws(() => parseZip321("https://example"));
  assert.throws(() => zip321Uri("ua with space"));
  const many = zip321UriMany([
    { address: "uregtest1aaa", amountZec: "0.1", memo: "one" },
    { address: "uregtest1bbb", amountZec: "0.2", memo: "two" },
  ]);
  assert.ok(many.startsWith("zcash:uregtest1aaa?"));
  assert.ok(many.includes("address.1=uregtest1bbb"));
  const p3 = parseZip321(many);
  assert.equal(p3.payments.length, 2);
  assert.equal(p3.payments[1].address, "uregtest1bbb");
  assert.equal(p3.payments[1].amountZat, 20_000_000n);
  assert.equal(p3.payments[1].memo, "two");
  const qaddr = parseZip321("zcash:?address=uregtest1abc&amount=1");
  assert.equal(qaddr.address, "uregtest1abc");
  assert.throws(() => parseZip321("zcash:uregtest1aaa?address.2=uregtest1bbb"));
  assert.throws(() => parseZip321("zcash:uregtest1aaa?req-expiry=1"));
  assert.throws(() => zip321UriMany([]));
});

test("parseBirthdayInput digits or date", () => {
  assert.equal(parseBirthdayInput("1687104", 3_000_000), 1687104);
  const h = parseBirthdayInput("2022-05-31", 3_000_000);
  assert.ok(h < 3_000_000);
  const exact = heightFromDate("2022-05-31", 3_000_000);
  assert.equal(h, Math.max(1, exact - DATE_SAFETY_BLOCKS));
  assert.throws(() => parseBirthdayInput("2024-02-31", 3_000_000));
  assert.throws(() => parseBirthdayInput("2023-02-29", 3_000_000));
  assert.ok(parseBirthdayInput("2024-02-29", 3_000_000) > 1);
  assert.equal(parseBirthdayInput("auto", 1000), 900);
});

function entry(partial: Partial<HistoryEntry>): HistoryEntry {
  return {
    txid: "aa",
    status: "mined",
    minedHeight: 10,
    expiryHeight: null,
    accountDeltaZat: 0,
    spentZat: 0,
    receivedZat: 0,
    feeZat: null,
    sentNoteCount: 0,
    receivedNoteCount: 0,
    memoCount: 0,
    hasChange: false,
    isShielding: false,
    expiredUnmined: false,
    memos: [],
    ...partial,
  };
}

test("classifyHistory: sent / received / shield / self-send", () => {
  const received = classifyHistory(
    entry({
      accountDeltaZat: 250_000,
      receivedZat: 250_000,
      orchardReceived: 250_000,
      receivedNoteCount: 1,
      memos: ["hi"],
    }),
  );
  assert.equal(received.action, "received");
  assert.equal(received.displayZat, 250_000n);
  assert.equal(received.memos[0], "hi");

  const sent = classifyHistory(
    entry({
      accountDeltaZat: -60_000,
      spentZat: 60_000,
      receivedZat: 0,
      orchardSpent: 60_000,
      feeZat: 10_000,
      sentNoteCount: 1,
    }),
  );
  assert.equal(sent.action, "sent");
  assert.equal(sent.displayZat, 50_000n);
  assert.equal(sent.feeZat, 10_000n);

  const shield = classifyHistory(
    entry({
      accountDeltaZat: -10_000,
      spentZat: 110_000,
      receivedZat: 100_000,
      transparentSpent: 110_000,
      orchardReceived: 100_000,
      feeZat: 10_000,
      isShielding: true,
    }),
  );
  assert.equal(shield.action, "shielding");
  assert.equal(shield.displayZat, 100_000n);

  const selfSend = inferTransactionType({
    netValue: -10_000n,
    fee: 10_000n,
    transparentSpent: 0n,
    transparentReceived: 0n,
    saplingSpent: 110_000n,
    saplingReceived: 0n,
    orchardSpent: 0n,
    orchardReceived: 100_000n,
    ironwoodSpent: 0n,
    ironwoodReceived: 0n,
  });
  assert.equal(selfSend, "internal");
});

test("history audit: linked transparent output is deshielded then shielded", () => {
  const deshield = entry({
    txid: "source", spentZat: 1_756_929, receivedZat: 0,
    accountDeltaZat: -115_000, orchardSpent: 1_756_929,
    orchardReceived: 1_641_929, feeZat: 15_000,
    historyMetadataComplete: true,
    transparentOutputs: [{ index: 0, valueZat: 100_000 }],
  });
  const shield = entry({
    txid: "target", spentZat: 0, receivedZat: 85_000,
    accountDeltaZat: 85_000, orchardReceived: 85_000,
    historyMetadataComplete: true,
    transparentInputs: [{ txid: "source", index: 0 }],
  });
  const [first, second] = classifyHistoryList([deshield, shield]);
  assert.deepEqual([first.action, first.displayZat, first.feeZat], ["deshielding", 100_000n, 15_000n]);
  assert.deepEqual([second.action, second.displayZat, second.feeZat], ["shielding", 85_000n, 15_000n]);
  const unrelated = classifyHistoryList([shield])[0];
  assert.equal(unrelated.action, "received", "an unknown transparent input is not evidence of shielding");
});

test("history audit: exact chain fees distinguish outgoing value, fee-only self sends and incoming", () => {
  const rows = classifyHistoryList([
    entry({ txid: "external", spentZat: 1_621_929, receivedZat: 606_929,
      accountDeltaZat: -1_015_000, orchardSpent: 1_621_929, orchardReceived: 606_929,
      feeZat: 15_000, historyMetadataComplete: true,
      transparentOutputs: [{ index: 0, valueZat: 1_000_000 }] }),
    entry({ txid: "shielded", spentZat: 2_036_929, accountDeltaZat: -2_036_929,
      orchardSpent: 2_036_929, feeZat: 15_000, historyMetadataComplete: true,
      outgoingShieldedZat: 2_021_929 }),
    entry({ txid: "self", spentZat: 1_641_929, receivedZat: 0,
      accountDeltaZat: -1_641_929, orchardSpent: 1_641_929, orchardReceived: 1_631_929,
      feeZat: 10_000, historyMetadataComplete: true }),
    entry({ txid: "incoming", receivedZat: 2_016_929, accountDeltaZat: 2_016_929,
      orchardReceived: 2_016_929, feeZat: 10_000, historyMetadataComplete: true }),
  ]);
  assert.deepEqual(rows.map((r) => [r.action, r.displayZat, r.feeZat]), [
    ["sent", 1_000_000n, 15_000n],
    ["sent", 2_021_929n, 15_000n],
    ["internal", 0n, 10_000n],
    ["received", 2_016_929n, null],
  ]);
});

test("canSend / canShield", () => {
  assert.equal(canShield(99_999), false);
  assert.equal(canShield(100_000), true);
  const bal = { saplingAvailable: 0, orchardAvailable: 60_000, ironwoodAvailable: 0 };
  assert.equal(canSend(bal, 50_000n), true);
  assert.equal(canSend(bal, 55_000n), false);
  assert.equal(canSendReason({ balance: bal, amountZat: 50_000n }).code, "ok");
  assert.equal(canSendReason({ balance: bal, amountZat: 0n }).code, "invalid_amount");
  assert.equal(canSendReason({ balance: bal, amountZat: 55_000n }).code, "insufficient_funds");
  assert.equal(canSendReason({ balance: bal, amountZat: 1n, viewOnly: true }).code, "view_only");
  assert.equal(canSendReason({ balance: bal, amountZat: 1n, spendReady: false }).code, "not_spend_ready");
  assert.equal(canSendReason({ balance: bal, amountZat: 1n, hasSeed: false }).code, "no_seed");
  const rows = [
    { txid: "aa", status: "mined" as const, minedHeight: 2, expiryHeight: null, accountDeltaZat: 1, spentZat: 0, receivedZat: 1, feeZat: null, sentNoteCount: 0, receivedNoteCount: 1, memoCount: 0, hasChange: false, isShielding: false, expiredUnmined: false },
    { txid: "bb", status: "pending" as const, minedHeight: null, expiryHeight: 9, accountDeltaZat: 1, spentZat: 0, receivedZat: 1, feeZat: null, sentNoteCount: 0, receivedNoteCount: 1, memoCount: 0, hasChange: false, isShielding: false, expiredUnmined: false },
    { txid: "cc", status: "expired" as const, minedHeight: null, expiryHeight: 1, accountDeltaZat: 1, spentZat: 0, receivedZat: 1, feeZat: null, sentNoteCount: 0, receivedNoteCount: 1, memoCount: 0, hasChange: false, isShielding: false, expiredUnmined: true },
  ];
  assert.deepEqual(filterHistory(rows, { status: "pending" }).map((r) => r.txid), ["bb"]);
  assert.deepEqual(filterHistory(rows, { txid: "CC" }).map((r) => r.txid), ["cc"]);
});

test("syncTuning: gRPC-Web uses smaller unary batches than native local", () => {
  const local = syncTuning(true);
  const web = syncTuning(true, { grpcWeb: true });
  const pipe = syncTuning(true, { lwdPipe: true });
  assert.equal(local.batch, 4_000);
  assert.equal(local.prefetch, 4);
  assert.equal(web.batch, 1_000);
  assert.equal(web.prefetch, 8);
  assert.equal(pipe.batch, 2_000);
  assert.equal(pipe.prefetch, 4);
  assert.equal(pipe.persistEvery, 8);
  assert.equal(pipe.batch * pipe.persistEvery, 16_000, "checkpoint span stays bounded");
  assert.equal(pipe.batch * pipe.prefetch, 8_000, "in-flight height lead stays bounded");
  assert.equal(pipe.batch * prefetchBuffer(pipe.prefetch), 16_000, "total reserved height lead stays bounded");
});

test("pipePrefetch is 4, or 2 on constrained radio", () => {
  assert.equal(pipePrefetch(), 4);
  assert.equal(pipePrefetch(false), 4);
  assert.equal(pipePrefetch(true), 2);
});

test("grpcWebPrefetch follows threads with a cap", () => {
  assert.equal(grpcWebPrefetch(1), 8);
  assert.equal(grpcWebPrefetch(8), 8);
  assert.equal(grpcWebPrefetch(12), 12);
  assert.equal(grpcWebPrefetch(32), 16);
  assert.equal(grpcWebPrefetch(8, true), 4);
});

test("near-tip catch-up is not 4%", () => {
  const origin = 3_472_263;
  const tip = 3_472_317;
  const pct = catchUpPercent(origin, origin, origin, tip);
  assert.ok(pct >= 90, `54 behind must not look like 4%, got ${pct}`);
  assert.equal(displayCatchUpPercent(4, origin, tip), 90);
  assert.ok(catchUpPercent(1, 1, 1, 3_000_000) < 0.5);
});

test("island gap fill is honest session percent", () => {
  const birthday = 3_335_466;
  const origin = 3_418_128;
  const scanned = 3_418_238;
  const tip = 3_472_935;
  const idle = catchUpPercent(origin, origin, origin, tip, birthday);
  assert.ok(idle < 0.5, `54k island→tip must not open at 90%, got ${idle}`);
  const pct = catchUpPercent(origin, scanned, origin, tip, birthday);
  assert.ok(pct > idle && pct < 10, `54k gap after 110 scanned is session percent, got ${pct}`);
  assert.ok(catchUpPercent(birthday, birthday, birthday, tip, birthday) < 0.5);
  const moved = catchUpPercent(birthday, birthday + 4000, birthday + 8000, tip, birthday);
  assert.ok(
    moved > 0.5 && moved < 10,
    `birthday catch-up must leave 0% once heights move, got ${moved}`,
  );
});

test("prefetch buffer is 2x so apply cannot stall HTTP", () => {
  assert.equal(prefetchBuffer(8), 16);
  assert.equal(prefetchBuffer(4), 8);
});

test("thirteen behind is not a dead light URL", () => {
  assert.equal(lightStallWarning(13, 13, true, false), false);
  assert.equal(lightStallWarning(10_000, 13, true, false), false);
  assert.equal(lightStallWarning(10_000, 100, true, true), false);
  assert.equal(lightStallWarning(10_000, 100, true, false), true);
});

test("six behind is a quiet remainder", () => {
  assert.ok(QUIET_BEHIND_BLOCKS >= 6);
  assert.ok(QUIET_BEHIND_BLOCKS < STALL_QUIET_REMAINING);
});

test("historic overlay hides only a near-tip remainder", () => {
  assert.equal(historicOverlayVisible(null), true);
  assert.equal(historicOverlayVisible(108_222), true);
  assert.equal(historicOverlayVisible(33), true);
  assert.equal(historicOverlayVisible(STALL_QUIET_REMAINING), false);
  assert.equal(historicOverlayVisible(0), false);
});

test("live ETA waits for movement and skips a 54k gap", () => {
  assert.equal(liveScanEtaSecs(0, 54_749, 3_418_128, 3_335_466, 400), undefined);
  assert.equal(liveScanEtaSecs(1, 54_749, 3_418_128, 3_335_466, 400), undefined);
  assert.equal(liveScanEtaSecs(120, 54_749, 3_418_128, 3_335_466, 400), undefined);
  assert.equal(liveScanEtaSecs(10, 40, 3_472_900, 3_472_900, 0.5), undefined);
  assert.equal(liveScanEtaSecs(10, 40, 3_472_900, 3_472_900, 10), 4);
});

test("a pending broadcast's txid is a typed WalletError field; another tab's save is wallet_changed", () => {
  const txid = "ab".repeat(32);
  const err = new WalletError("broadcast_failed", "broadcast outcome unknown", undefined, { txid });
  assert.equal(err.txid, txid);
  assert.equal(new WalletError("broadcast_failed", "no txid").txid, undefined);
  assert.equal(classifyWalletError("the wallet was saved by another tab"), "wallet_changed");
  assert.equal(classifyWalletError("wallet changed in another tab; reload before continuing"), "wallet_changed");
});

test("WalletError maps known engine strings and leaves unknown intact", () => {
  assert.equal(classifyWalletError("insufficient funds"), "insufficient_funds");
  assert.equal(classifyWalletError("propose_transfer: Insufficient funds"), "insufficient_funds");
  assert.equal(classifyWalletError("sync required before this operation"), "sync_required");
  assert.equal(
    classifyWalletError(
      "seed unlock required (passphrase, OS keychain / Hello, or paste the words)",
    ),
    "seed_locked",
  );
  assert.equal(
    classifyWalletError("this wallet is view-only — paste the seed to send"),
    "view_only",
  );
  assert.equal(
    classifyWalletError("transparent send is not supported; shield first"),
    "unsupported_transparent",
  );
  assert.equal(
    classifyWalletError("wasm send is orchard-only; sapling destinations need the desktop wallet"),
    "unsupported_destination",
  );
  assert.equal(
    classifyWalletError("bridge token required — paste the token printed by `z-wallet serve`"),
    "auth",
  );
  assert.equal(classifyWalletError("ledger_status_6a80: Ledger rejected the PCZT data or key path"), "hardware_rejected");
  assert.equal(classifyWalletError("some novel engine panic"), "unknown");
  const e = WalletError.fromMessage("insufficient funds");
  assert.equal(e.code, "insufficient_funds");
  assert.equal(e.name, "WalletError");
  assert.equal(e.userMessage(), walletErrorMessage("insufficient_funds"));
  assert.equal(WalletError.fromUnknown(e), e);
  assert.equal(WalletError.fromUnknown(new Error("no wasm wallet")).code, "not_found");
  assert.match(walletErrorMessage("wallet_db"), /wipe scan/i);
  assert.equal(walletErrorMessage("unknown", "raw dump"), "raw dump");
});

test("UA receiver sets are full / orchard / shielded", () => {
  assert.equal(DEFAULT_UA_RECEIVER_SET, "full");
  assert.deepEqual([...uaReceivers("full")], ["orchard", "sapling", "p2pkh"]);
  assert.deepEqual([...uaReceivers("orchard")], ["orchard"]);
  assert.deepEqual([...uaReceivers("shielded")], ["orchard", "sapling"]);
  assert.equal(parseUaReceiverSet("all"), "full");
  assert.equal(parseUaReceiverSet("ORCHARD"), "orchard");
  assert.equal(uaIncludesTransparent("full"), true);
  assert.equal(uaIncludesTransparent("orchard"), false);
  assert.equal(classifyUaReceiverSet(["orchard", "sapling", "p2pkh"]), "full");
  assert.equal(classifyUaReceiverSet(["orchard"]), "orchard");
  assert.equal(classifyUaReceiverSet(["sapling"]), null);
  assert.throws(() => parseUaReceiverSet("transparent"));
  assert.match(uaReceiverSetLabel("full"), /t receive/i);
  assert.equal(
    inspectAddressSummary({
      network: "regtest",
      kind: "unified",
      receivers: ["orchard", "sapling", "p2pkh"],
      receiverSet: "full",
    }),
    "unified · Full (+t receive) · orchard + sapling + p2pkh",
  );
});

test("a different recovery phrase has a stable actionable error without exposing the secret", () => {
  const error = WalletError.fromMessage("those words do not match this wallet's viewing key (account index 0)");
  assert.equal(error.code, "seed_mismatch");
  assert.equal(error.userMessage(), "This recovery phrase does not match your wallet.");
  assert.equal(classifyWalletError("Ledger seed fingerprint does not match this viewing key"), "hardware_mismatch");
});


test("birthday validation rejects malformed dates and unsafe heights before network work", async () => {
  const { validateBirthdayInput, parseBirthdayInput, WalletError } = await import("../src/index.ts");
  for (const value of ["yesterday", "2026-13-40", "2025-02-29", "2026-1-01", "1e5", 0, -1, 1.5, NaN, Infinity, 0x1_0000_0000]) {
    assert.throws(() => validateBirthdayInput(value), error => error instanceof WalletError && error.code === "invalid_birthday");
  }
  for (const value of [1, "4468500", "2024-02-29", "auto", ""]) validateBirthdayInput(value);
  assert.equal(parseBirthdayInput("4468500", 4476000), 4468500);
});
