import assert from "node:assert/strict";
import { test } from "node:test";
import { syncPublicData, supportsTransparentCompact } from "../src/public-data.ts";
import type { ScanSession } from "../src/scan-host.ts";
import { grpcWebTransport } from "../src/lwd.ts";
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

test("an overlong public compact response cannot apply deposits or advance coverage", async (t) => {
  const realFetch = globalThis.fetch;
  t.after(() => { globalThis.fetch = realFetch; });
  const f = fixture(1);
  const transport = grpcWebTransport("https://gateway.example");
  transport.info = f.transport.info;
  let cancelled = false, positiveApplies = 0;
  const checkpoints: Array<number | null> = [];
  const bytes = new Uint8Array(10); // Two empty, well-framed gRPC data messages for a one-block request.
  globalThis.fetch = (async () => new Response(new ReadableStream<Uint8Array>({
    pull(controller) { controller.enqueue(bytes); },
    cancel() { cancelled = true; },
  }, { highWaterMark: 0 }))) as typeof fetch;
  const apply = f.source.applyTransparentBlocks!;
  f.source.applyTransparentBlocks = async (blob) => { if (blob.length) positiveApplies++; return apply(blob); };
  await assert.rejects(syncPublicData({ ...f.opts, transport, memos: false,
    checkpoint: async () => { checkpoints.push(f.state.transparentScanHeight); },
  }), /message count limit/);
  assert.equal(cancelled, true);
  assert.equal(positiveApplies, 0);
  assert.equal(f.state.transparentScanHeight, null);
  assert.deepEqual(checkpoints, [null], "only the existing pre-download incomplete-coverage barrier is checkpointed");
});

for (const method of ["info", "transparentBlocks", "sharedMemos"] as const) {
  test(`public ${method} read cancels promptly even when the provider ignores its signal`, async () => {
    const f = fixture();
    let ready!: () => void;
    const started = new Promise<void>(resolve => { ready = resolve; });
    let release!: () => void;
    const answer = new Promise<void>(resolve => { release = resolve; });
    let observed: AbortSignal | undefined;
    if (method === "info") f.transport.info = async signal => { observed = signal; ready(); await answer; return { chain: "main", protocolVersion: "v0.5.0", transparentCompact: true }; };
    if (method === "transparentBlocks") f.transport.transparentBlocks = async (_start, end, signal) => { observed = signal; ready(); await answer; const bytes = new Uint8Array(4); new DataView(bytes.buffer).setUint32(0, end); return bytes; };
    if (method === "sharedMemos") f.transport.sharedMemos = async (start, end, signal) => { observed = signal; ready(); await answer; return JSON.stringify({ start, end }); };
    const pending = syncPublicData({ ...f.opts, transparent: method !== "sharedMemos", memos: method === "sharedMemos" });
    const rejected = assert.rejects(pending, /fixture public read cancelled/);
    await started;
    const before = structuredClone(f.state);
    f.controller.abort(new Error("fixture public read cancelled"));
    try {
      await Promise.race([rejected, new Promise<never>((_, reject) => { setTimeout(() => reject(new Error("cancelled public read still waited for provider")), 500); })]);
      assert.equal(observed, f.controller.signal);
    } finally { release(); await pending.catch(() => {}); }
    await new Promise(resolve => setImmediate(resolve));
    assert.deepEqual(f.state, before);
  });
}

test("compact backfill honors the cap without changing memo ranges", async () => {
  const f = fixture(12);
  assert.deepEqual(await syncPublicData({ ...f.opts, maxBlocksPerRequest: 3 }), { transparent: "complete", memos: "complete" });
  assert.deepEqual(f.calls, [["blocks", 1, 3], ["blocks", 4, 6], ["blocks", 7, 9], ["blocks", 10, 12],
    ["memos", 1, 9], ["memos", 10, 12]]);
  assert.equal(f.state.transparentScanHeight, 12);
});

for (const [cap, expected] of [[3, [null, 5001, 10_001]], [1000, [null, 5000, 10_000, 10_001]]] as const) {
  test(`compact backfill keeps checkpoint cadence with cap ${cap}`, async () => {
    const f = fixture(10_001), checkpoints: Array<number | null> = [];
    await syncPublicData({ ...f.opts, memos: false, maxBlocksPerRequest: cap,
      checkpoint: async () => { checkpoints.push(f.state.transparentScanHeight); } });
    assert.deepEqual(checkpoints, expected);
    assert.equal(f.state.transparentScanHeight, 10_001);
  });
}

test("compact backfill unsafe cap refuses before snapshot, mutation or provider", async () => {
  const f = fixture();
  let calls = 0;
  f.source.snapshotJson = async () => { calls++; return "{}"; };
  await assert.rejects(syncPublicData({ ...f.opts, maxBlocksPerRequest: 0 }), /maxBlocksPerRequest/);
  assert.equal(calls, 0);
  assert.deepEqual(f.calls, []);
  assert.equal(f.state.transparentScanHeight, null);
});

for (const [cap, expected] of [
  [1, [["memos", 1, 1], ["memos", 2, 2], ["memos", 3, 3], ["memos", 4, 4], ["memos", 5, 5], ["memos", 6, 6], ["memos", 7, 7], ["memos", 8, 8], ["memos", 9, 9], ["memos", 10, 10], ["memos", 11, 11], ["memos", 12, 12]]],
  [3, [["memos", 1, 3], ["memos", 4, 6], ["memos", 7, 9], ["memos", 10, 12]]],
  [10, [["memos", 1, 9], ["memos", 10, 12]]],
] as const) {
  test(`shared memo cap ${cap} bounds provider ranges and commits every completed range`, async () => {
    const f = fixture(12), checkpoints: Array<number | null> = [];
    assert.deepEqual(await syncPublicData({ ...f.opts, transparent: false,
      maxSharedMemoBlocksPerRequest: cap, checkpoint: async () => { checkpoints.push(f.state.memoScanHeight); },
    }), { memos: "complete" });
    assert.deepEqual(f.calls, expected);
    assert.deepEqual(checkpoints, expected.map(([, , end]) => end));
  });
}

test("shared memo cap refuses before reading or mutating wallet state", async () => {
  const f = fixture();
  let reads = 0;
  f.source.snapshotJson = async () => { reads++; return "{}"; };
  await assert.rejects(syncPublicData({ ...f.opts, maxSharedMemoBlocksPerRequest: 0 }), /maxSharedMemoBlocksPerRequest/);
  assert.equal(reads, 0);
  assert.deepEqual(f.calls, []);
  assert.equal(f.state.memoScanHeight, null);
});

test("one-block memo retrieval resumes only after its last durable checkpoint", async () => {
  const f = fixture(12);
  let saved: number | null = null;
  const fetch = f.transport.sharedMemos!;
  f.transport.sharedMemos = async (start, end, signal) => {
    if (start === 5) throw new TypeError("fixture shared range unavailable");
    return fetch(start, end, signal);
  };
  const options = { ...f.opts, transparent: false, maxSharedMemoBlocksPerRequest: 1,
    checkpoint: async () => { saved = f.state.memoScanHeight; },
  };
  await assert.rejects(syncPublicData(options), /fixture shared range unavailable/);
  assert.equal(saved, 4);
  f.state.memoScanHeight = saved;
  f.calls.length = 0;
  f.transport.sharedMemos = fetch;
  assert.deepEqual(await syncPublicData(options), { memos: "complete" });
  assert.deepEqual(f.calls[0], ["memos", 5, 5]);
  assert.equal(saved, 12);
});

test("memo caps preserve common boundaries when coverage resumes inside a bucket", async () => {
  const f = fixture(14);
  f.state.birthdayHeight = 8;
  assert.deepEqual(await syncPublicData({ ...f.opts, transparent: false, maxSharedMemoBlocksPerRequest: 4 }), { memos: "complete" });
  assert.deepEqual(f.calls, [["memos", 8, 9], ["memos", 10, 13], ["memos", 14, 14]]);
});
