import assert from "node:assert/strict";
import { test } from "node:test";
import { minimumConfirmations, confirmationBalance } from "../src/confirmation-policy.ts";
import { createEngineClient } from "../src/engine.ts";
import { localScanSession } from "../src/scan-host.ts";

const numericBalance = { saplingAvailable: 0, orchardAvailable: 1_010_000, ironwoodAvailable: 0,
  transparentAvailable: 0, totalAvailable: 1_010_000, saplingPending: 0, orchardPending: 12,
  ironwoodPending: 0, transparentPending: 0, totalPending: 12 };

test("confirmation policies accept exactly positive u32 integers", () => {
  assert.equal(minimumConfirmations(undefined), undefined);
  for (const count of [1, 6, 0xffffffff]) assert.equal(minimumConfirmations(count), count);
  for (const count of [0, -1, 1.5, NaN, Infinity, 0x100000000, "6", null])
    assert.throws(() => minimumConfirmations(count), /minConfirmations must be an integer/);
});

test("confirmation balances expose exact numeric and formatted public fields", () => {
  const balance = confirmationBalance(JSON.stringify(numericBalance));
  assert.equal(balance.orchardAvailable, 1_010_000);
  assert.equal(balance.totalZec, "0.01010000");
  assert.equal(balance.orchardPendingZec, "0.00000012");
  assert.equal(balance.totalPending, 12);
  for (const changed of [{ orchardAvailable: -1 }, { totalAvailable: 99 }, { totalPending: 13 },
    { orchardAvailable: 2_100_000_000_000_001 }, { orchardAvailable: "1010000" }])
    assert.throws(() => confirmationBalance(JSON.stringify({ ...numericBalance, ...changed })), /invalid engine balance/);
});

test("native bridge rejects a confirmation policy before making a request", async () => {
  const client = createEngineClient("http://127.0.0.1:1");
  await assert.rejects(client.balance({ minConfirmations: 6 }), /require the browser software engine/);
  await assert.rejects(client.estimateFee("recipient", "1", undefined, { minConfirmations: 6 }), /require the browser software engine/);
  await assert.rejects(client.send("recipient", "1", undefined, { minConfirmations: 6 }), /require the browser software engine/);
  await assert.rejects(client.balance({ minConfirmations: 0 }), /minConfirmations must be an integer/);
  await assert.rejects(client.estimateFee("recipient", "1", undefined, { minConfirmations: 0x100000000 }), /minConfirmations must be an integer/);
  await assert.rejects(client.send("recipient", "1", undefined, { minConfirmations: 1.5 }), /minConfirmations must be an integer/);
});

function scanFixture() {
  const calls: unknown[][] = [];
  const handle = {
    applyCompactBlocks: () => "{}", nextUnifiedAddress: () => "address", snapshotJson: () => "{}",
    toSnapshot: () => new Uint8Array(), history: () => "[]", scannedHeight: () => 100,
    birthday: () => 10, nextHeight: () => 101, transparentAddress: () => undefined,
    rewindTo(height: number) { calls.push(["old-rewind", height]); return height; },
    rewindToChecked(height: number) { calls.push(["checked-rewind", height]); return height; },
    historyQuery(limit: number, status: string, txid: string) { calls.push(["history", limit, status, txid]); return "[]"; },
    balanceWithConfirmations(count: number) { calls.push(["balance", count]); return JSON.stringify(numericBalance); },
    estimateFee(to: string, amount: string, memo?: string, count?: number) {
      calls.push(["fee", to, amount, memo, count]); return '{"feeZat":10000}';
    },
  } satisfies Parameters<typeof localScanSession>[0];
  return { handle, calls };
}

test("scan session forwards exact confirmation, filtered history, and checked rewind policy", async () => {
  const { handle, calls } = scanFixture(), session = localScanSession(handle);
  await session.balanceWithConfirmations(6);
  await session.estimateFee("recipient", "0.01", "memo", 6);
  await session.historyQuery(1, "mined", "ab".repeat(32));
  assert.equal(session.persistenceRevision?.(), 2, "fee proposal is the only mutation before checked rewind");
  await session.rewindToChecked(90);
  assert.deepEqual(calls, [["balance", 6], ["fee", "recipient", "0.01", "memo", 6],
    ["history", 1, "mined", "ab".repeat(32)], ["checked-rewind", 90]]);
  await assert.rejects(session.balanceWithConfirmations(0), /minConfirmations/);
  await assert.rejects(session.estimateFee("recipient", "0.01", undefined, Infinity), /minConfirmations/);
  await assert.rejects(session.rewindToChecked(-1), /invalid rewind height/);
  assert.equal(calls.length, 4);
});

test("older bindings cannot silently ignore a requested policy or checked rewind", async () => {
  const { handle, calls } = scanFixture();
  const { balanceWithConfirmations, rewindToChecked, historyQuery, ...older } = handle;
  const session = localScanSession(older);
  await assert.rejects(session.balanceWithConfirmations(6), /cannot enforce confirmation/);
  await assert.rejects(session.estimateFee("recipient", "0.01", undefined, 6), /cannot enforce confirmation/);
  await assert.rejects(session.rewindToChecked(90), /cannot verify a rewind checkpoint/);
  await assert.rejects(session.historyQuery(1, "", "ab".repeat(32)), /cannot query complete transaction history/);
  assert.deepEqual(calls, []);
  await session.estimateFee("recipient", "0.01");
  assert.deepEqual(calls, [["fee", "recipient", "0.01", undefined, undefined]]);
});
