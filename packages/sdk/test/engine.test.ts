import assert from "node:assert/strict";
import { after, test } from "node:test";
import { createEngineClient } from "../src/engine.ts";
import { createNativeWallet } from "../src/native.ts";
import { canShield, canSendReason, parseZecToZatoshis } from "@z-stack/core";
import { REGTEST_FAUCET_MNEMONIC, REGTEST_FAUCET_TRANSPARENT } from "../src/constants.ts";
import { allowsTransparentQuery, httpLwdTransport, isLoopbackUrl, isPublicLwdUrl, isTransientLightServerError, looksLikeGrpcWeb, looksLikeLwdPipe, PIPE_HTTP_MAX, usesFastSync } from "../src/lwd.ts";
import {
  LOCAL_ZAINO_GRPC,
  LOCAL_ZAINO_GRPC_MAINNET,
  LOCAL_ZAKURA_RPC_TESTNET,
  SDK_VERSION,
  createNativeClient,
  localEndpoints,
  WalletError,
} from "../src/lab.ts";
import { peekSnapshotBytes, reorgRestartFrom } from "../src/wasm-client.ts";
import { HASH_KEEP } from "@z-stack/core";

const origFetch = globalThis.fetch;
const calls: Array<{ url: string; method: string; body: string | undefined; auth?: string }> = [];

after(() => {
  globalThis.fetch = origFetch;
});

function jsonOk(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

test("native restore rejects browser-only transaction hooks before any request", async () => {
  let requests = 0;
  globalThis.fetch = (async () => { requests++; return jsonOk({}); }) as typeof fetch;
  const engine = createEngineClient("http://127.0.0.1:8787");
  for (const method of ["restore", "restoreUfvk"] as const) {
    for (const options of [
      { signal: new AbortController().signal },
      { assertCurrent() {} },
      { beforeCommit() {} },
    ]) {
      await assert.rejects(engine[method]("fixture", "regtest", 1, options), /guarded restore requires browser restoreUfvk/);
    }
  }
  assert.equal(requests, 0);
});

test("native send refuses browser cancellation and review hooks before contacting the bridge", async () => {
  let requests = 0;
  globalThis.fetch = (async () => { requests++; return jsonOk({}); }) as typeof fetch;
  const engine = createEngineClient("http://127.0.0.1:8787");
  for (const options of [
    { signal: new AbortController().signal },
    { beforeBroadcast: () => true },
    { onStage() {} },
  ]) {
    await assert.rejects(engine.send("fixture", "0.01", undefined, options), /require the browser engine/);
  }
  assert.equal(requests, 0);
});

test("createEngineClient routes every SDK op", async () => {
  calls.length = 0;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? init.body : undefined;
    const headers = new Headers(init?.headers);
    calls.push({ url, method, body, auth: headers.get("authorization") ?? undefined });
    if (url.endsWith("/health")) return jsonOk({ ok: true, wallet: true, bind: "loopback", mode: "native-bridge" });
    if (url.endsWith("/wallet")) {
      return jsonOk({
        network: "regtest",
        server: "http://127.0.0.1:28137",
        birthdayHeight: 1,
        unifiedAddress: "uregtest1abc",
        transparentAddress: REGTEST_FAUCET_TRANSPARENT,
        zip321: "zcash:uregtest1abc",
        scannedHeight: 10,
        balance: {
          saplingAvailable: 0,
          orchardAvailable: 50_000,
          ironwoodAvailable: 0,
          transparentAvailable: 0,
          totalAvailable: 50_000,
          saplingZec: "0",
          orchardZec: "0.0005",
          ironwoodZec: "0",
          transparentZec: "0",
          totalZec: "0.0005",
        },
      });
    }
    if (
      url.endsWith("/sync") ||
      url.endsWith("/shield") ||
      url.endsWith("/create") ||
      url.endsWith("/restore") ||
      url.endsWith("/address/next") ||
      url.endsWith("/attach-seed") ||
      url.endsWith("/unlock-policy") ||
      url.endsWith("/scan/reset")
    ) {
      return jsonOk({ unifiedAddress: "uregtest1abc", txids: ["aa"] });
    }
    if (url.includes("/history")) {
      const status = new URL(url).searchParams.get("status");
      if (status === "pending") return jsonOk({ transactions: [{ txid: "pend", status: "pending" }] });
      return jsonOk({ transactions: [{ txid: "ab", status: "mined" }] });
    }
    if (url.includes("/tx?")) return jsonOk({ txid: "ab", status: "mined", minedHeight: 1 });
    if (url.endsWith("/tip")) return jsonOk({ tip: 20, scanned: 20, behind: 0 });
    if (url.endsWith("/send")) return jsonOk({ txids: ["cc"] });
    if (url.endsWith("/send/estimate")) return jsonOk({ feeZat: 10_000, feeZec: "0.0001" });
    if (url.endsWith("/send/max")) return jsonOk({ maxSendZat: 40_000, maxSendZec: "0.0004", feeZat: 10_000, feeZec: "0.0001" });
    if (url.endsWith("/address/inspect")) {
      return jsonOk({
        network: "regtest",
        kind: "unified",
        receivers: ["orchard", "sapling", "p2pkh"],
        receiverSet: "full",
      });
    }
    return jsonOk({ error: "nope" }, 404);
  }) as typeof fetch;

  assert.equal(createNativeClient, createEngineClient);
  assert.match(SDK_VERSION, /^\d+\.\d+\.\d+/);
  const c = createEngineClient("http://127.0.0.1:18787/", { token: "secret-token" });
  assert.equal(c.baseUrl, "http://127.0.0.1:18787");
  assert.equal((await c.health()).ok, true);
  assert.equal((await c.getWallet()).unifiedAddress, "uregtest1abc");
  await c.sync();
  await c.shield(100_000);
  await c.send("uregtest1xyz", "0.0005", "hi");
  await c.create("regtest", 1, { passphrase: "test-pass" });
  await c.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1);
  await c.restoreUfvk("uview1abc", "regtest", "2022-05-31");
  await c.attachSeed(REGTEST_FAUCET_MNEMONIC);
  await c.setUnlockPolicy("session");
  assert.equal((await c.history(3))[0].txid, "ab");
  assert.equal((await c.pending(5))[0].txid, "pend");
  assert.equal((await c.transaction("ab"))?.txid, "ab");
  assert.equal((await c.tip()).behind, 0);
  await c.nextAddress();
  await c.resetScan();
  const fee = await c.estimateFee("uregtest1xyz", "0.0005");
  assert.equal(fee.feeZat, 10_000);
  const max = await c.maxSend();
  assert.equal(max.maxSendZat, 40_000);
  const inspected = await c.inspectAddress("uregtest1abc");
  assert.equal(inspected.receiverSet, "full");
  await c.waitUntilCaughtUp({ timeoutMs: 1_000, intervalMs: 10 });

  const paths = calls.map((x) => x.method + " " + x.url.replace("http://127.0.0.1:18787", ""));
  for (const need of [
    "GET /health",
    "GET /wallet",
    "POST /sync",
    "POST /shield",
    "POST /send",
    "POST /create",
    "POST /restore",
    "POST /attach-seed",
    "POST /unlock-policy",
    "GET /history?limit=3",
    "GET /tip",
    "POST /address/next",
    "POST /scan/reset",
    "POST /send/estimate",
    "POST /send/max",
    "POST /address/inspect",
  ]) {
    assert.ok(paths.includes(need), `missing ${need} in ${paths.join(", ")}`);
  }
  const send = calls.find((c) => c.url.endsWith("/send"));
  assert.ok(send?.body?.includes("\"memo\":\"hi\""));
  const created = calls.find((x) => x.url.endsWith("/create"));
  assert.ok(created?.body?.includes("test-pass"));
  assert.equal(send?.auth, "Bearer secret-token");
  const restoreUfvk = calls.find((x) => x.url.endsWith("/restore") && x.body?.includes("uview1abc"));
  assert.ok(restoreUfvk?.body?.includes("uview1abc"));

  calls.length = 0;
  const zip =
    "zcash:uregtest1aaa?amount=0.1&address.1=uregtest1bbb&amount.1=0.2";
  await c.send(zip, "", undefined);
  const zipSend = calls.find((x) => x.url.endsWith("/send"));
  assert.ok(zipSend?.body?.includes("zcash:uregtest1aaa"));
  assert.ok(zipSend?.body?.includes("address.1=uregtest1bbb"));
});

test("native restores send the requested unlock policy in the same request and adopt the saved policy", async () => {
  const requests: Array<{ path: string; body: Record<string, unknown> }> = [];
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const path = new URL(String(input)).pathname;
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    requests.push({ path, body });
    if (path === "/unlock-policy") return jsonOk({ error: "settings save failed" }, 500);
    return jsonOk({ unifiedAddress: "uregtest1fixture", unlockPolicy: body.unlockPolicy,
      balance: { totalAvailable: 0 } });
  }) as typeof fetch;
  const c = createEngineClient("http://127.0.0.1:8787");
  const observedPolicies: string[] = [];
  c.on("balance", () => observedPolicies.push(c.unlockPolicy()));
  const restored = await c.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1, { unlockPolicy: "each-spend" });
  assert.equal(restored.unlockPolicy, "each-spend");
  assert.equal(c.unlockPolicy(), "each-spend");
  await c.restoreUfvk("uview1fixture", "regtest", 1, { unlockPolicy: "always" });
  assert.equal(c.unlockPolicy(), "always");
  assert.deepEqual(observedPolicies, ["each-spend", "always"], "subscribers see the policy of the snapshot they received");
  assert.deepEqual(requests.map(({ path, body }) => [path, body.unlockPolicy]), [
    ["/restore", "each-spend"], ["/restore", "always"],
  ]);
  await assert.rejects(c.setUnlockPolicy("session"), /settings save failed/);
  assert.equal(c.unlockPolicy(), "always", "failed writes must not change the reported policy");
});

test("native wallet snapshot and pure spending checks", async () => {
  globalThis.fetch = (async () =>
    jsonOk({
      network: "regtest",
      server: "x",
      birthdayHeight: 1,
      unifiedAddress: "u",
      transparentAddress: null,
      zip321: "zcash:u",
      balance: {
        saplingAvailable: 0,
        orchardAvailable: 50_000,
        ironwoodAvailable: 0,
        transparentAvailable: 200_000,
        totalAvailable: 250_000,
        saplingZec: "0",
        orchardZec: "0.0005",
        ironwoodZec: "0",
        transparentZec: "0.002",
        totalZec: "0.0025",
      },
    })) as typeof fetch;
  const w = createNativeWallet("http://127.0.0.1:9");
  const snap = await w.getWallet();
  assert.equal(canShield(snap.balance.transparentAvailable), true);
  assert.equal(canSendReason({ balance: snap.balance, amountZat: parseZecToZatoshis("0.0003") }).ok, true);
  assert.equal(canSendReason({ balance: snap.balance, amountZat: parseZecToZatoshis("0.0005") }).ok, false);
});

test("t-scan only when proxy and light server are loopback", () => {
  assert.equal(isLoopbackUrl("http://127.0.0.1:8787"), true);
  assert.equal(isLoopbackUrl("https://zec.rocks:443"), false);
  assert.equal(allowsTransparentQuery("http://127.0.0.1:28137"), true);
  assert.equal(allowsTransparentQuery("local"), true);
  assert.equal(allowsTransparentQuery("local-regtest"), true);
  assert.equal(allowsTransparentQuery("regtest"), false);
  assert.equal(allowsTransparentQuery("https://zec.rocks:443"), false);
  assert.equal(isPublicLwdUrl("https://zec.rocks:443"), true);
  assert.equal(isPublicLwdUrl("http://203.0.113.9:8137"), false);
  assert.equal(usesFastSync("http://203.0.113.9:8137"), true);
  assert.equal(usesFastSync("https://zec.rocks:443"), false);
  assert.equal(looksLikeGrpcWeb("http://127.0.0.1:1238"), true);
  assert.equal(looksLikeGrpcWeb("http://127.0.0.1:1234/zaino"), true);
  assert.equal(looksLikeGrpcWeb("https://zec.rocks:443"), true);
  assert.equal(looksLikeGrpcWeb("http://127.0.0.1:8137"), false);
  assert.equal(looksLikeGrpcWeb("http://127.0.0.1:1239"), false);
  assert.equal(looksLikeLwdPipe("http://127.0.0.1:1239"), true);
  assert.equal(looksLikeLwdPipe("http://127.0.0.1:1238"), false);
  const pipe = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138");
  assert.equal(pipe.kind, "lwd-pipe");
  assert.equal(typeof pipe.blockStream, "function");
  assert.equal(PIPE_HTTP_MAX, 8_000);
  const local = httpLwdTransport("http://127.0.0.1:8787", "regtest", "http://127.0.0.1:28137");
  assert.equal(typeof local.utxos, "function");
  const publicLwd = httpLwdTransport("http://127.0.0.1:8787", "mainnet", "https://zec.rocks:443");
  assert.equal(publicLwd.utxos, undefined);
  const remoteProxy = httpLwdTransport("https://example.invalid", "regtest", "http://127.0.0.1:28137");
  assert.equal(remoteProxy.utxos, undefined);
});

test("explainBridgeAuthError is about serve not Zaino", async () => {
  const { explainBridgeAuthError } = await import("../src/lwd.ts");
  const missing = explainBridgeAuthError(401, "bridge token required");
  assert.ok(missing?.includes("z-wallet serve"));
  assert.ok(missing?.includes("gRPC-Web") || missing?.includes("not Zaino"));
  const bad = explainBridgeAuthError(401, "bridge token invalid — serve prints a new token");
  assert.ok(bad?.includes("invalid"));
});

test("httpLwdTransport sends Bearer token", async () => {
  calls.length = 0;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const headers = new Headers(init?.headers);
    calls.push({ url, method: init?.method ?? "GET", body: undefined, auth: headers.get("authorization") ?? undefined });
    if (url.includes("/lwd/tip")) return jsonOk({ tip: 12 });
    if (url.includes("/lwd/blocks")) return new Response(new Uint8Array([0, 0, 0, 0]), { status: 200 });
    return jsonOk({ error: "nope" }, 404);
  }) as typeof fetch;

  const t = httpLwdTransport(
    "http://127.0.0.1:8787",
    "regtest",
    "http://127.0.0.1:28137",
    undefined,
    "secret-token",
  );
  assert.equal(await t.tip(), 12);
  assert.equal(calls[0]?.auth, "Bearer secret-token");
  assert.ok(calls[0]?.url.startsWith("http://127.0.0.1:8787/lwd/tip"));

  calls.length = 0;
  const fromUrl = httpLwdTransport("http://127.0.0.1:8787/?token=url-token", "regtest");
  await fromUrl.tip();
  assert.equal(calls[0]?.auth, "Bearer url-token");
  assert.ok(!calls[0]?.url.includes("token=url-token"));
});

test("createEngineClient reads token from URL query", async () => {
  calls.length = 0;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const headers = new Headers(init?.headers);
    calls.push({ url, method: init?.method ?? "GET", body: undefined, auth: headers.get("authorization") ?? undefined });
    if (url.endsWith("/health")) return jsonOk({ ok: true, wallet: true, bind: "loopback" });
    return jsonOk({ error: "nope" }, 404);
  }) as typeof fetch;
  const c = createEngineClient("http://127.0.0.1:18787/?token=url-secret");
  assert.equal(c.baseUrl, "http://127.0.0.1:18787");
  assert.equal((await c.health()).ok, true);
  assert.equal(calls[0]?.auth, "Bearer url-secret");
  assert.equal(calls[0]?.url, "http://127.0.0.1:18787/health");
});

test("localEndpoints keeps Zaino and Zakura independent", () => {
  const t = localEndpoints("testnet");
  assert.equal(t.light, LOCAL_ZAINO_GRPC);
  assert.equal(t.validatorRpc, LOCAL_ZAKURA_RPC_TESTNET);
  assert.notEqual(t.light, t.validatorRpc);
  assert.equal(localEndpoints("regtest").validatorRpc, "http://127.0.0.1:29232");
  assert.equal(localEndpoints("mainnet").light, LOCAL_ZAINO_GRPC_MAINNET);
});

test("pipe blocks() pages at PIPE_HTTP_MAX", async () => {
  calls.length = 0;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    calls.push({ url, method: init?.method ?? "GET", body: undefined });
    return new Response(new Uint8Array([1]), { status: 200 });
  }) as typeof fetch;
  const t = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138");
  const blob = await t.blocks(1, PIPE_HTTP_MAX + 10);
  assert.equal(blob.byteLength, 2);
  assert.ok(calls.some((c) => c.url.includes("start=1") && c.url.includes(`end=${PIPE_HTTP_MAX}`)));
  assert.ok(
    calls.some((c) => c.url.includes(`start=${PIPE_HTTP_MAX + 1}`) && c.url.includes(`end=${PIPE_HTTP_MAX + 10}`)),
  );
});

test("a compact-block response that stops delivering becomes a retryable outage", async () => {
  let requests = 0;
  globalThis.fetch = (async () => {
    requests++;
    // Headers, then nothing: a half-open pipe.
    return new Response(new ReadableStream<Uint8Array>({ start() {} }), { status: 200 });
  }) as typeof fetch;
  const t = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138", undefined, undefined, { blockIdleMs: 40 });
  const error = await t.blocks(1, 10).catch((e: unknown) => e);
  assert.ok(isTransientLightServerError(error), String(error));
  assert.match(String(error), /stalled/);
  assert.equal(requests, 3, "the same range is retried before the outer outage grace takes over");
});

test("a slow compact-block response that keeps delivering is not cut off", async () => {
  globalThis.fetch = (async () => {
    let sent = 0;
    const body = new ReadableStream<Uint8Array>({
      async pull(controller) {
        await new Promise((r) => setTimeout(r, 25));
        if (sent++ < 6) controller.enqueue(new Uint8Array([sent]));
        else controller.close();
      },
    });
    return new Response(body, { status: 200 });
  }) as typeof fetch;
  // Six gaps of 25 ms take longer than the 40 ms idle limit in total.
  const t = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138", undefined, undefined, { blockIdleMs: 40 });
  const blob = await t.blocks(1, 10);
  assert.deepEqual([...blob], [1, 2, 3, 4, 5, 6]);
});

test("pipe blocks() one GET when the span is PIPE_HTTP_MAX", async () => {
  calls.length = 0;
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    calls.push({ url: String(input), method: "GET", body: undefined });
    return new Response(new Uint8Array([1]), { status: 200 });
  }) as typeof fetch;
  const t = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138");
  await t.blocks(3_368_292, 3_368_292 + PIPE_HTTP_MAX - 1);
  assert.equal(calls.length, 1);
  assert.ok(calls[0]?.url.includes("start=3368292"));
  assert.ok(calls[0]?.url.includes(`end=${3_368_292 + PIPE_HTTP_MAX - 1}`));
});

test("pipe blocks() fetches overlap when callers do not await serially", async () => {
  calls.length = 0;
  let live = 0;
  let maxLive = 0;
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    live += 1;
    maxLive = Math.max(maxLive, live);
    calls.push({ url: String(input), method: "GET", body: undefined });
    await new Promise((r) => setTimeout(r, 25));
    live -= 1;
    return new Response(new Uint8Array([1]), { status: 200 });
  }) as typeof fetch;
  const t = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138");
  await Promise.all([t.blocks(1, 1000), t.blocks(1001, 2000), t.blocks(2001, 3000)]);
  assert.ok(maxLive >= 3, `expected overlapping GETs, maxLive ${maxLive}`);
});

test("lwd-pipe mempool 404 is empty and is not retried", async () => {
  calls.length = 0;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    calls.push({ url, method: init?.method ?? "GET", body: undefined, auth: undefined });
    if (url.includes("/lwd/mempool")) return jsonOk({ error: "unknown lwd-pipe route" }, 404);
    return jsonOk({ error: "nope" }, 404);
  }) as typeof fetch;
  const t = httpLwdTransport("http://127.0.0.1:1239", "mainnet", "http://127.0.0.1:8138");
  assert.deepEqual(await t.mempool!(), []);
  assert.deepEqual(await t.mempool!(), []);
  assert.equal(calls.filter((c) => c.url.includes("/lwd/mempool")).length, 1);
});

test("peekSnapshotBytes paints UA and balance without wasm", () => {
  const bytes = new TextEncoder().encode(
    JSON.stringify({
      magic: "zstk1",
      network: "mainnet",
      unifiedAddress: "u1peek",
      transparentAddress: "t1peek",
      birthday: 100,
      scannedHeight: 200,
      treesReady: true,
      notes: [
        { pool: "orchard", valueZat: 50_000, spent: false },
        { pool: "orchard", valueZat: 10_000, spent: true },
      ],
      utxos: [{ valueZat: 1_000, spent: false }],
    }),
  );
  const w = peekSnapshotBytes(bytes);
  assert.ok(w);
  assert.equal(w.unifiedAddress, "u1peek");
  assert.equal(w.scannedHeight, 200);
  assert.equal(w.balance.orchardAvailable, 50_000);
  assert.equal(w.balance.transparentAvailable, 1_000);
  assert.equal(w.balance.totalAvailable, 51_000);
  assert.equal(w.treesReady, true);
});

test("peekSnapshotBytes reads UA from truncated zstk1 JSON", () => {
  const bytes = new TextEncoder().encode(
    '{"magic":"zstk1","network":"mainnet","ufvk":"uview1abc","unifiedAddress":"u1peek","birthday":100,"scannedHeight":200,"notes":[',
  );
  const w = peekSnapshotBytes(bytes);
  assert.ok(w);
  assert.equal(w.unifiedAddress, "u1peek");
  assert.equal(w.ufvk, "uview1abc");
  assert.equal(w.birthdayHeight, 100);
  assert.equal(w.scannedHeight, 200);
  assert.equal(w.balance.totalAvailable, 0);
});

test("peekSnapshotBytes counts notes in a large zstk1 header", () => {
  const head = JSON.stringify({
    magic: "zstk1",
    network: "mainnet",
    unifiedAddress: "u1peek",
    birthday: 3_368_308,
    scannedHeight: 3_472_882,
    orchardAvailable: 701_929,
    totalAvailable: 701_929,
    notes: [
      { pool: "orchard", valueZat: 200_000, spent: false },
      { pool: "orchard", valueZat: 200_000, spent: false },
      { pool: "orchard", valueZat: 200_000, spent: false },
      { pool: "orchard", valueZat: 101_929, spent: false },
    ],
  }).slice(0, -1);
  const bytes = new TextEncoder().encode(`${head},"pad":"${"x".repeat(5 * 1024 * 1024)}"}`);
  assert.ok(bytes.byteLength > 4 * 1024 * 1024);
  const w = peekSnapshotBytes(bytes);
  assert.ok(w);
  assert.equal(w.balance.orchardAvailable, 701_929);
  assert.equal(w.balance.totalAvailable, 701_929);
});

test("peekSnapshotBytes uses header totals when notes sit past 512KB", () => {
  const head =
    '{"magic":"zstk1","network":"mainnet","unifiedAddress":"u1peek","birthday":1,"scannedHeight":2,"orchardAvailable":701929,"totalAvailable":701929,"pad":"';
  const bytes = new TextEncoder().encode(
    `${head}${"x".repeat(5 * 1024 * 1024)}","notes":[{"pool":"orchard","valueZat":1,"spent":false}]}`,
  );
  assert.ok(bytes.byteLength > 4 * 1024 * 1024);
  const w = peekSnapshotBytes(bytes);
  assert.ok(w);
  assert.equal(w.balance.orchardAvailable, 701_929);
  assert.equal(w.balance.totalAvailable, 701_929);
});

test("reorgRestartFrom uses engine rescan-from, not birthday", () => {
  assert.equal(reorgRestartFrom(3_472_830, 3_472_820, 3_372_308), 3_472_820);
  assert.throws(
    () => reorgRestartFrom(3_472_830, 3_372_307, 3_372_308),
    /snapshot was not overwritten/,
  );
  assert.ok(3_472_830 - 3_372_307 > HASH_KEEP);
});

test("isTreeConflictError surfaces Wipe scan & resync", async () => {
  const { isTreeConflictError, treeConflictUserMessage } = await import("../src/scan-host.ts");
  const raw =
    "selective-scan insert_tree: Inserted root conflicts with existing root at address Address { level: Level(0), index: 50081105 }";
  assert.equal(isTreeConflictError(raw), true);
  assert.match(treeConflictUserMessage(raw), /Wipe scan & resync/);
  assert.equal(isTreeConflictError("scan worker applyBlob timed out"), false);
});

test("createNativeClient does not require initialize", () => {
  assert.equal(createNativeClient, createEngineClient);
  const c = createNativeClient("http://127.0.0.1:9");
  assert.equal(typeof c.sync, "function");
  assert.equal(typeof c.on, "function");
  assert.equal(typeof c.estimateFee, "function");
  assert.equal(typeof c.maxSend, "function");
  assert.equal(typeof c.inspectAddress, "function");
});

test("EngineClient on/off emits sync and balance", async () => {
  globalThis.fetch = (async () =>
    jsonOk({
      network: "regtest",
      server: "x",
      birthdayHeight: 1,
      unifiedAddress: "u",
      transparentAddress: null,
      zip321: "zcash:u",
      scannedHeight: 10,
      balance: {
        saplingAvailable: 0,
        orchardAvailable: 50_000,
        ironwoodAvailable: 0,
        transparentAvailable: 0,
        totalAvailable: 50_000,
        saplingZec: "0",
        orchardZec: "0.0005",
        ironwoodZec: "0",
        transparentZec: "0",
        totalZec: "0.0005",
      },
    })) as typeof fetch;
  const c = createEngineClient("http://127.0.0.1:9");
  const syncs: string[] = [];
  const bals: number[] = [];
  const offSync = c.on("sync", (e) => syncs.push(e.stage));
  c.on("balance", (e) => bals.push(e.availableZat));
  await c.sync();
  assert.ok(syncs.includes("connecting"));
  assert.ok(syncs.includes("synced"));
  assert.deepEqual(bals, [50_000]);
  offSync();
  c.off("balance", (e) => bals.push(e.availableZat));
  await c.sync();
  assert.equal(bals.length, 2);
});

test("native client maps bridge errors to WalletError codes", async () => {
  globalThis.fetch = (async () =>
    jsonOk({ error: "insufficient funds" }, 400)) as typeof fetch;
  const c = createEngineClient("http://127.0.0.1:9");
  await assert.rejects(
    () => c.send("uregtest1abc", "1"),
    (e: unknown) => {
      assert.ok(e instanceof WalletError);
      assert.equal(e.code, "insufficient_funds");
      return true;
    },
  );
});


test("native transparent swap methods fail without contacting the bridge", async () => {
  let requests = 0;
  globalThis.fetch = (async () => { requests++; return jsonOk({}); }) as typeof fetch;
  const engine = createEngineClient("http://127.0.0.1:8787");
  assert.equal(await engine.supportsTransparentSend(), false);
  await assert.rejects(engine.estimateTransparentFee("recipient", "0.1"), /WASM software wallet/);
  await assert.rejects(engine.sendTransparent("recipient", "0.1"), /WASM software wallet/);
  assert.equal(requests, 0);
});


test("native creation rejects browser recovery preparation before any request", async () => {
  let requests = 0;
  globalThis.fetch = (async () => { requests++; return jsonOk({}); }) as typeof fetch;
  await assert.rejects(createEngineClient("http://127.0.0.1:8787").create("regtest", 1, {
    beforeCommit() {},
  }), /creation preparation requires the browser engine/);
  assert.equal(requests, 0);
});
