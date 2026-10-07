import assert from "node:assert/strict";
import { test } from "node:test";
import { syncPublicData, supportsTransparentCompact } from "../src/public-data.ts";
import type { ScanSession } from "../src/scan-host.ts";
import type { BlockTransport } from "../src/lwd.ts";

function fixture(tip = 12) {
  const state = { birthdayHeight: 1, scannedHeight: tip, transparentScanHeight: null as number | null, memoScanHeight: null as number | null };
  const calls: unknown[] = [];
  const source = {
    snapshotJson: async () => JSON.stringify(state),
    applyTransparentBlocks: async (bytes: Uint8Array) => {
      if (bytes.length) state.transparentScanHeight = new DataView(bytes.buffer).getUint32(0);
      return 0;
    },
    applySharedMemos: async (json: string) => { state.memoScanHeight = JSON.parse(json).end; return 0; },
  } as unknown as ScanSession;
  const transport = {
    info: async () => ({ chain: "main", protocolVersion: "v0.5.0", transparentCompact: true }),
    transparentBlocks: async (start: number, end: number) => {
      calls.push(["blocks", start, end]);
      const bytes = new Uint8Array(4); new DataView(bytes.buffer).setUint32(0, end); return bytes;
    },
    sharedMemos: async (start: number, end: number) => { calls.push(["memos", start, end]); return JSON.stringify({ start, end }); },
    utxos: async () => { throw new Error("address lookup forbidden"); },
    tx: async () => { throw new Error("selective memo lookup forbidden"); },
  } as unknown as BlockTransport;
  const controller = new AbortController();
  const opts = { source, transport, transparent: true, memos: true, signal: controller.signal, assertCurrent: () => {}, checkpoint: async () => {} };
  return { state, calls, source, transport, controller, opts };
}

test("public retrieval negotiates exact release versions and never silently falls back", async () => {
  for (const version of [undefined, "", "v0.4.1", "v0.5.0-preview", "banana"]) assert.equal(supportsTransparentCompact(version), false);
  for (const version of ["v0.5.0", "0.6.0", "1.0.0"]) assert.equal(supportsTransparentCompact(version), true);
  const f = fixture();
  f.transport.info = async () => ({ chain: "main", protocolVersion: "v0.4.0" });
  f.transport.sharedMemos = async () => null;
  assert.deepEqual(await syncPublicData(f.opts), { transparent: "unsupported", memos: "unsupported" });
  assert.deepEqual(f.calls, []);
});

test("public retrieval scans complete shared ranges and resumes coverage without wallet IDs", async () => {
  const f = fixture();
  assert.deepEqual(await syncPublicData(f.opts), { transparent: "complete", memos: "complete" });
  assert.deepEqual(f.calls, [["blocks", 1, 12], ["memos", 1, 9], ["memos", 10, 12]]);
  f.state.scannedHeight = 15; f.calls.length = 0;
  await syncPublicData(f.opts);
  assert.deepEqual(f.calls, [["blocks", 13, 15], ["memos", 13, 15]]);
});

test("backfill has independent bounded progress and checks response ranges before mutation", async () => {
  const f = fixture(50_000);
  assert.deepEqual(await syncPublicData(f.opts), { transparent: "scanning", memos: "scanning" });
  assert.equal(f.state.transparentScanHeight, 20_000);
  assert.equal(f.state.memoScanHeight, 100);
  f.transport.sharedMemos = async () => JSON.stringify({ start: 1, end: 9 });
  await assert.rejects(syncPublicData({ ...f.opts, transparent: false }), /Wrong shared/);
  assert.equal(f.state.memoScanHeight, 100);
});

test("deposit backfill resumes from its durable checkpoint after a network interruption", async () => {
  const f = fixture(25_000);
  let savedHeight: number | null = null;
  const checkpoints: Array<number | null> = [];
  const blocks = f.transport.transparentBlocks!;
  f.transport.transparentBlocks = async (start, end, signal) => {
    if (start === 8001) throw new TypeError("network disconnected");
    return blocks(start, end, signal);
  };
  const options = { ...f.opts, memos: false, checkpoint: async () => {
    savedHeight = f.state.transparentScanHeight;
    checkpoints.push(savedHeight);
  } };
  await assert.rejects(syncPublicData(options), /network disconnected/);
  assert.equal(f.state.transparentScanHeight, 8000);
  assert.equal(savedHeight, 5000);
  assert.deepEqual(checkpoints, [null, 5000]);
  f.state.transparentScanHeight = savedHeight; // A fresh session hydrates the last commit.
  f.transport.transparentBlocks = blocks;
  f.calls.length = 0;
  assert.deepEqual(await syncPublicData(options), { transparent: "complete" });
  assert.deepEqual(f.calls[0], ["blocks", 5001, 6000]);
  assert.equal(savedHeight, 25_000);
});

test("cancellation and changed sessions prevent late public-data applies", async () => {
  for (const cancel of [true, false]) {
    const f = fixture(); let changed = false;
    f.transport.sharedMemos = async (start, end) => {
      if (cancel) f.controller.abort(); else changed = true;
      return JSON.stringify({ start, end });
    };
    await assert.rejects(syncPublicData({ ...f.opts, transparent: false,
      assertCurrent: () => { if (changed) throw new Error("changed"); } }));
    assert.equal(f.state.memoScanHeight, null);
  }
});

test("completed memo coverage retries a failed final checkpoint without refetching", async () => {
  const f = fixture(9);
  let attempts = 0, persistedHeight: number | null = null;
  const options = { ...f.opts, transparent: false, checkpoint: async () => {
    if (++attempts < 3) throw new Error("transient storage failure");
    persistedHeight = f.state.memoScanHeight;
  } };
  await assert.rejects(syncPublicData(options), /storage failure/);
  assert.equal(f.state.memoScanHeight, 9);
  assert.equal(persistedHeight, null);
  await assert.rejects(syncPublicData(options), /storage failure/);
  assert.deepEqual(await syncPublicData(options), { memos: "complete" });
  assert.equal(persistedHeight, 9);
  assert.deepEqual(f.calls, [["memos", 1, 9]], "checkpoint retries disclose no additional request");
});
