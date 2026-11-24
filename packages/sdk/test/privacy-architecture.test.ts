import test from "node:test";
import assert from "node:assert/strict";
import {
  BroadcastRejection,
  HopError,
  WalletError,
  alignedCoverBucket,
  createHopEnhancePirTransport,
  createHopTransparentPirTransport,
  createSplitHopRouter,
  directHop,
  grpcWebTransport,
  nymHop,
  ohttpHop,
  resolveMemoPolicy,
  type HopRequest,
  type NetworkHop,
} from "../src/index.ts";
import { createSpendingController } from "../src/wasm-client-spending.ts";

test("alignedCoverBucket computes aligned ranges, shifts backwards near tip, and never shrinks to (h, h)", () => {
  assert.deepEqual(alignedCoverBucket(123, 200, 10), { start: 121, end: 130 });
  assert.deepEqual(alignedCoverBucket(198, 200, 10), { start: 191, end: 200 });
  // Near chain tip at bucket start: MUST shift backwards to full width, NEVER (91, 91)!
  assert.deepEqual(alignedCoverBucket(91, 91, 10), { start: 82, end: 91 });
  assert.deepEqual(alignedCoverBucket(92, 95, 10), { start: 86, end: 95 });
  assert.deepEqual(alignedCoverBucket(5, 8, 10), { start: 1, end: 8 });
  assert.deepEqual(alignedCoverBucket(2_500_123, 3_400_000, 500), { start: 2_500_001, end: 2_500_500 });
  assert.throws(() => alignedCoverBucket(0, 100, 10), /Invalid mined height/);
  assert.throws(() => alignedCoverBucket(10, 100, 1), /cover bucket size/);
});

test("resolveMemoPolicy defaults to pir-only when Enhance PIR is present", () => {
  assert.equal(resolveMemoPolicy(undefined, true), "pir-only");
  assert.equal(resolveMemoPolicy(undefined, false), "on-demand");
  assert.equal(resolveMemoPolicy("auto", true), "pir-only");
  assert.equal(resolveMemoPolicy("shared", false), "shared-sequential");
  assert.equal(resolveMemoPolicy("pir-plus-shared-tip", true), "pir-plus-shared-tip");
  assert.throws(() => resolveMemoPolicy("invalid" as never, true), /Unsupported memoFetch policy/);
});

test("ohttpHop wraps any inner hop, sends message/ohttp-req to relay, and fails closed on inner errors", async () => {
  const seenInner: HopRequest[] = [];
  const inner: NetworkHop = {
    kind: "nym",
    label: "mock-nym",
    async exchange(req) {
      seenInner.push(req);
      return { status: 200, headers: {}, body: new Uint8Array([9, 8, 7]) };
    },
    close() {},
  };
  let sealedTarget = "";
  const wrapped = ohttpHop({
    relayUrl: "https://relay.example/ohttp",
    inner,
    sealer: {
      async seal(req) {
        sealedTarget = req.target;
        return {
          requestBody: new Uint8Array([1, 2, 3]),
          responseLimit: 1024,
          finish(enc) {
            return { status: 200, headers: { "x-ok": "1" }, body: enc };
          },
          cancel() {},
        };
      },
    },
  });

  assert.equal(wrapped.kind, "ohttp");
  assert.equal(wrapped.label, "OHTTP via mock-nym");
  const res = await wrapped.exchange(
    {
      method: "POST",
      target: "/sync/tip",
      headers: { "content-type": "application/octet-stream" },
      body: new Uint8Array([42]),
      role: "read",
    },
    new AbortController().signal,
  );
  assert.equal(res.status, 200);
  assert.deepEqual(res.body, new Uint8Array([9, 8, 7]));
  assert.equal(sealedTarget, "/sync/tip");
  assert.equal(seenInner.length, 1);
  assert.equal(seenInner[0]!.target, "https://relay.example/ohttp");
  assert.equal(seenInner[0]!.headers?.["content-type"], "message/ohttp-req");
  assert.equal(seenInner[0]!.idempotent, false, "inner sealed OHTTP envelope must never be replayed");

  // Non-HTTPS non-loopback relay URLs must be rejected.
  assert.throws(
    () =>
      ohttpHop({
        relayUrl: "http://evil.example/ohttp",
        inner,
        sealer: { seal: async () => { throw new Error("unreachable"); } },
      }),
    (err: unknown) => err instanceof HopError && err.code === "invalid_request",
  );
});

test("nymHop fails closed on every role when mixnet cannot connect", async () => {
  const disconnected = nymHop({
    connect: async () => {
      throw new Error("mixnet offline");
    },
  });
  for (const role of ["read", "submit", "pir", "external"] as const) {
    await assert.rejects(
      disconnected.exchange(
        { method: "GET", target: "https://example.test", role },
        new AbortController().signal,
      ),
      (err: unknown) => err instanceof HopError && err.code === "fail_closed",
    );
  }
});

test("createSplitHopRouter retries idempotent reads, NEVER retries submit, and enforces epoch fencing on closeAll", async () => {
  let readCalls = 0;
  let submitCalls = 0;
  const readHop: NetworkHop = {
    kind: "ohttp",
    label: "read-ohttp",
    async exchange() {
      readCalls++;
      if (readCalls < 2) throw new HopError("unavailable", "503 temporary");
      return { status: 200, headers: {}, body: new Uint8Array([1]) };
    },
    close() {},
  };
  const submitHop: NetworkHop = {
    kind: "nym",
    label: "submit-nym",
    async exchange() {
      submitCalls++;
      throw new HopError("timeout", "timeout after send");
    },
    close() {},
  };

  const router = createSplitHopRouter({
    defaultHop: readHop,
    readHop,
    submitHop,
    retry: { maxAttempts: 3, baseDelayMs: 1, maxDelayMs: 5 },
  });

  // Idempotent read retries and succeeds.
  const ok = await router.exchange(
    { method: "GET", target: "/tip", role: "read" },
    new AbortController().signal,
  );
  assert.equal(ok.status, 200);
  assert.equal(readCalls, 2);

  // Submit MUST NEVER retry even on retryable transport errors!
  await assert.rejects(
    router.exchange(
      { method: "POST", target: "/submit", role: "submit" },
      new AbortController().signal,
    ),
    (err: unknown) => err instanceof HopError && err.code === "timeout",
  );
  assert.equal(submitCalls, 1);

  // Closing advances epoch and fences out further requests.
  const beforeEpoch = router.epoch;
  await router.closeAll();
  assert.equal(router.epoch, beforeEpoch + 1);
  await assert.rejects(
    router.exchange(
      { method: "GET", target: "/tip", role: "read" },
      new AbortController().signal,
    ),
    (err: unknown) => err instanceof HopError && err.code === "closed",
  );
});

test("createHopEnhancePirTransport and createHopTransparentPirTransport match exact Rust wire protocol", async () => {
  const seen: HopRequest[] = [];
  const hop: NetworkHop = {
    kind: "ohttp",
    label: "pir-ohttp",
    async exchange(req) {
      seen.push(req);
      if (req.target.endsWith("/stale-trigger")) {
        return { status: 409, headers: {}, body: new TextEncoder().encode(JSON.stringify({ map_sha256: "cd".repeat(32) })) };
      }
      return { status: 200, headers: {}, body: new Uint8Array([77, 88]) };
    },
    close() {},
  };
  const enhance = createHopEnhancePirTransport({
    hop,
    baseUrl: "https://pir.example",
  });
  const enhOut = await enhance.exchange(
    { kind: "query", body: new Uint8Array([1, 2]), anchor: { height: 123, hash: "aa".repeat(32) } },
    new AbortController().signal,
  );
  assert.deepEqual(enhOut, { status: 200, body: new Uint8Array([77, 88]) });
  assert.equal(seen[0]!.headers?.["x-enhance-anchor-height"], "123");
  assert.equal(seen[0]!.headers?.["x-enhance-anchor-hash"], "aa".repeat(32));

  const transparent = createHopTransparentPirTransport({
    hop,
    baseUrl: "https://tpir.example",
    sourceId: "ab".repeat(32),
  });
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const trInit = await transparent.exchange(
    { operation: enc.encode(JSON.stringify({ kind: "init" })) },
    new AbortController().signal,
  );
  assert.deepEqual(JSON.parse(dec.decode(trInit)), { kind: "data", body: btoa(String.fromCharCode(77, 88)) });

  const trSetup = await transparent.exchange(
    {
      operation: enc.encode(
        JSON.stringify({
          kind: "setup",
          shard_id: 4,
          revision: "rev-1",
          table: { kind: "pages", child: 2 },
          segment: 7,
        }),
      ),
    },
    new AbortController().signal,
  );
  assert.equal(JSON.parse(dec.decode(trSetup)).kind, "data");

  await transparent.exchange(
    {
      operation: enc.encode(
        JSON.stringify({
          kind: "query",
          shard_id: 4,
          revision: "rev-1",
          table: { kind: "directory" },
          body: btoa("hello"),
        }),
      ),
    },
    new AbortController().signal,
  );

  assert.deepEqual(
    seen.map((r) => [r.role, r.method, r.target]),
    [
      ["pir", "POST", "https://pir.example/v1/enhance/query"],
      ["pir", "GET", "https://tpir.example/v2/shards/init"],
      ["pir", "GET", "https://tpir.example/v2/shards/4/revisions/rev-1/setup/pages-2/7"],
      ["pir", "POST", "https://tpir.example/v2/shards/4/revisions/rev-1/query/directory"],
    ],
  );
  assert.equal(seen[3]!.headers?.["content-type"], "application/octet-stream");
  assert.deepEqual(seen[3]!.body, enc.encode("hello"));
});

test("grpcWebTransport routes reads and submits over a composable NetworkHop", async () => {
  const seen: HopRequest[] = [];
  const hop: NetworkHop = {
    kind: "ohttp",
    label: "mock-hop",
    async exchange(req) {
      seen.push(req);
      // Encode a minimal BlockID protobuf { height: 42 } (field 1 varint = 0x08 0x2a) in a gRPC-Web data frame
      const payload = new Uint8Array([0x08, 0x2a]);
      const frame = new Uint8Array(5 + payload.length);
      frame[0] = 0x00;
      frame[4] = payload.length;
      frame.set(payload, 5);
      return { status: 200, headers: { "grpc-status": "0" }, body: frame };
    },
    close() {},
  };
  const transport = grpcWebTransport("https://lwd.example", { hop });
  const tip = await transport.tip(new AbortController().signal);
  assert.equal(tip, 42);
  assert.equal(seen.length, 1);
  assert.equal(seen[0]!.role, "read");
  assert.equal(seen[0]!.idempotent, true);
  assert.equal(seen[0]!.target, "https://lwd.example/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock");
});

test("two-step ZIP-320 TEX submission handles step-1 duplicate acceptance and preserves reservation on step-2 rejection", async () => {
  const step1 = { txid: "11".repeat(32), hex: "aa" };
  const step2 = { txid: "22".repeat(32), hex: "bb" };
  const g = globalThis as unknown as { __zStackWasm?: unknown };
  const origBindings = g.__zStackWasm;
  g.__zStackWasm = { consensusBranchId: () => "c8e71055" };

  const makeCtx = (submit: (hex: string, signal?: AbortSignal, context?: { txid: string }) => Promise<string>) => {
    const rolledBack: string[] = [];
    const session = {
      kind: "main" as const,
      scannedHeight: async () => 100,
      toSnapshot: async () => new Uint8Array([1]),
      fromSnapshot: async () => {},
      prove: async () => ({ hex: step2.hex, txid: step2.txid, steps: [step1, step2] }),
      hardware: async (op: string, arg?: { txid?: string }) => {
        if (op === "rollback" && arg?.txid) rolledBack.push(arg.txid);
        return "";
      },
    };
    const ctx = {
      network: "regtest" as const,
      sessionOperation: null,
      currentSession: () => session,
      loadIfNeeded: async () => true,
      session,
      snap: async () => ({} as never),
      assertSource: () => {},
      disposing: false,
      refreshIfStale: async () => {},
      transport: {
        kind: "mock",
        label: "mock",
        tip: async () => 100,
        blocks: async () => new Uint8Array(),
        info: async () => ({ consensusBranchId: "c8e71055", blockHeight: 100 }),
        submit,
      },
      stateEpoch: 1,
      savedKeySeen: null,
      persist: async () => {},
      beginReservationDrain: () => () => {},
      adoptSaved: async () => {},
      bus: { emit: () => {} },
      unlockPolicy: "session" as const,
      spendingSeed: null,
      disposeWaits: new AbortController(),
      storageGeneration: 1,
      bindLocal: async () => {},
      applyLoaded: async () => {},
      copyLegacy: async () => {},
      refreshMempool: async () => {},
    };
    return { ctx, rolledBack };
  };

  try {
    // Case A: Step 1 returns "already in mempool" (accepted verdict) -> MUST still submit Step 2!
    const submittedA: string[] = [];
    const receiptsA: Array<{ txid: string; txids?: string[] }> = [];
    const { ctx: ctxA, rolledBack: rbA } = makeCtx(async (_hex, _sig, c) => {
      submittedA.push(c!.txid);
      if (c!.txid === step1.txid) throw new BroadcastRejection(-27, "transaction already in mempool");
      return step2.txid;
    });
    const ctrlA = createSpendingController(ctxA as never);
    const outA = await ctrlA.proveAndSubmit(
      "sendTex",
      "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
      { to: "texregtest1example", amountZec: "0.1" },
      undefined,
      async (r) => { receiptsA.push(r); },
    );
    assert.equal(outA, step2.txid);
    assert.deepEqual(submittedA, [step1.txid, step2.txid]);
    assert.deepEqual(receiptsA, [{ txid: step2.txid, txids: [step1.txid, step2.txid] }]);
    assert.deepEqual(rbA, []);

    // Case B: Step 1 succeeds, Step 2 is rejected -> MUST NOT roll back and MUST throw broadcast_failed!
    const { ctx: ctxB, rolledBack: rbB } = makeCtx(async (_hex, _sig, c) => {
      if (c!.txid === step1.txid) return step1.txid;
      throw new BroadcastRejection(-25, "bad-txns-inputs-missingorspent");
    });
    const ctrlB = createSpendingController(ctxB as never);
    await assert.rejects(
      ctrlB.proveAndSubmit(
        "sendTex",
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        { to: "texregtest1example", amountZec: "0.1" },
      ),
      (err: unknown) => err instanceof WalletError && err.code === "broadcast_failed",
    );
    assert.deepEqual(rbB, [], "must never roll back after step 1 succeeded");

    // Case C: Step 1 is rejected -> rolls back BOTH step 2 and step 1 and throws broadcast_rejected!
    const { ctx: ctxC, rolledBack: rbC } = makeCtx(async () => {
      throw new BroadcastRejection(-25, "bad-txns-spent");
    });
    const ctrlC = createSpendingController(ctxC as never);
    await assert.rejects(
      ctrlC.proveAndSubmit(
        "sendTex",
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        { to: "texregtest1example", amountZec: "0.1" },
      ),
      (err: unknown) => err instanceof WalletError && err.code === "broadcast_rejected",
    );
    assert.deepEqual(rbC, [step2.txid, step1.txid], "both steps roll back in reverse order when step 1 is rejected");
  } finally {
    g.__zStackWasm = origBindings;
  }
});

test("directHop strips ambient headers and bounds response bodies", async () => {
  let seenHeaders: Record<string, string> | undefined;
  const hop = directHop({
    fetch: async (_url, init) => {
      seenHeaders = init?.headers as Record<string, string>;
      return new Response(new Uint8Array([5, 6]), { status: 200 });
    },
  });
  const res = await hop.exchange(
    {
      method: "GET",
      target: "https://example.test/x",
      headers: {
        Cookie: "secret=1",
        Origin: "https://evil.example",
        Referer: "https://evil.example/page",
        Accept: "application/octet-stream",
      },
      role: "read",
    },
    new AbortController().signal,
  );
  assert.equal(res.status, 200);
  assert.deepEqual(res.body, new Uint8Array([5, 6]));
  assert.deepEqual(seenHeaders, { accept: "application/octet-stream" });
});
