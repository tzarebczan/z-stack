import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import { createServer } from "node:http";
import { once } from "node:events";
import { createEngineClient } from "../src/engine.ts";
import { BroadcastRejection, decodeAddressUtxos, grpcWebTransport, httpLwdTransport, isTransientLightServerError } from "../src/lwd.ts";
import { syncPublicData } from "../src/public-data.ts";
import type { ScanSession } from "../src/scan-host.ts";

const realFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = realFetch; });

function frame(payload: Uint8Array, trailer = false): Uint8Array {
  const out = new Uint8Array(5 + payload.length);
  out[0] = trailer ? 0x80 : 0;
  new DataView(out.buffer).setUint32(1, payload.length);
  out.set(payload, 5);
  return out;
}

function reply(...messages: Uint8Array[]): Uint8Array {
  const ok = frame(new TextEncoder().encode("grpc-status:0\r\n"), true);
  return Buffer.concat([...messages.map((m) => frame(m)), ok]);
}

function mockFetch(handler: (method: string, body: Uint8Array) => Uint8Array) {
  const calls: Array<{ method: string; body: Uint8Array; keepalive?: boolean }> = [];
  globalThis.fetch = (async (url: string, init: RequestInit) => {
    const method = String(url).split("/").pop()!;
    const body = new Uint8Array(init.body as ArrayBuffer).subarray(5);
    calls.push({ method, body, keepalive: init.keepalive });
    return new Response(handler(method, body), { status: 200 });
  }) as typeof fetch;
  return calls;
}

test("subtree roots decode the Zakura protobuf fields for every shielded pool", async () => {
  // SubtreeRoot: field 1 reserved; root_hash=2, block_hash=3, height=4.
  const root = new Uint8Array(32).fill(17);
  const blockHash = new Uint8Array(32).fill(34);
  const response = new Uint8Array([0x12, 32, ...root, 0x1a, 32, ...blockHash, 0x20, 0xac, 0x02]);
  const calls = mockFetch(() => reply(response));
  const transport = grpcWebTransport("https://lwd.example");
  for (const protocol of ["sapling", "orchard", "ironwood"] as const) {
    assert.deepEqual(await transport.subtreeRoots!(protocol, 7, undefined, 8), [
      { completingHeight: 300, rootHash: "11".repeat(32) },
    ]);
  }
  assert.deepEqual(calls.map(c => [...c.body]), [
    [8, 7, 16, 0, 24, 8], [8, 7, 16, 1, 24, 8], [8, 7, 16, 2, 24, 8],
  ]);
  assert(calls.every(c => c.method === "GetSubtreeRoots"));
});

test("a malformed subtree fails the page instead of shifting subsequent shard indices", async () => {
  const valid = new Uint8Array([0x12, 32, ...new Uint8Array(32), 0x20, 1]);
  mockFetch(() => reply(valid, new Uint8Array([0x12, 1, 0]), valid));
  await assert.rejects(grpcWebTransport("https://lwd.example").subtreeRoots!("orchard"), /invalid subtree/);
});

test("subtree heights reject overflow and truncated varints before they can wrap to a valid height", async () => {
  const varint = (value: bigint) => {
    const bytes: number[] = [];
    do { const byte = Number(value & 127n); value >>= 7n; bytes.push(byte | (value ? 128 : 0)); } while (value);
    return bytes;
  };
  const response = (height: number[]) => new Uint8Array([0x12, 32, ...new Uint8Array(32), 0x20, ...height]);
  mockFetch(() => reply(response(varint(0xffff_ffffn))));
  assert.equal((await grpcWebTransport("https://lwd.example").subtreeRoots!("orchard"))[0].completingHeight, 0xffff_ffff);
  for (const height of [varint(0x1_0000_0001n), varint(1n << 53n), [0x81], new Array(11).fill(0x80)]) {
    mockFetch(() => reply(response(height)));
    await assert.rejects(grpcWebTransport("https://lwd.example").subtreeRoots!("orchard"), /subtree|protobuf/);
  }
});

test("gRPC-Web requests omit browser credentials and referrers without adding authentication", async () => {
  globalThis.fetch = (async (_url: unknown, init: RequestInit) => {
    assert.equal(init.credentials, "omit");
    assert.equal(init.referrerPolicy, "no-referrer");
    assert.equal(init.redirect, "error");
    assert.equal(new Headers(init.headers).has("authorization"), false);
    assert.equal(new Headers(init.headers).has("cookie"), false);
    assert.ok(init.signal instanceof AbortSignal);
    return new Response(reply(new Uint8Array([0x08, 0x0a])));
  }) as typeof fetch;
  assert.equal(await grpcWebTransport("https://lwd.example").tip(), 10);
});

test("unknown hosted and loopback servers do not probe the shared memo extension or fall back", async () => {
  let requests = 0;
  globalThis.fetch = (async () => { requests++; throw new Error("unexpected network request"); }) as typeof fetch;
  const source = {
    snapshotJson: async () => JSON.stringify({ birthdayHeight: 1, scannedHeight: 10 }),
    applySharedMemos: async () => { throw new Error("unexpected shared memo apply"); },
  } as unknown as ScanSession;
  for (const url of ["https://zcash-mainnet.chainsafe.dev", "https://zec.rocks", "http://127.0.0.1:28138"]) {
    const transport = grpcWebTransport(url);
    assert.equal(transport.sharedMemos, undefined);
    for (let pass = 0; pass < 2; pass++) {
      const result = await syncPublicData({ source, transport, transparent: false, memos: true,
        signal: new AbortController().signal, assertCurrent: () => {}, checkpoint: async () => {} });
      assert.deepEqual(result, { memos: "unsupported" });
    }
  }
  assert.equal(requests, 0, "no custom route, transaction ID or address request is permitted");
});

test("explicit shared memo gateways preserve range and browser privacy policies", async () => {
  const body = JSON.stringify({ start: 10, end: 19, blocks: "", transactions: [] });
  let requests = 0;
  globalThis.fetch = (async (url: string, init: RequestInit) => {
    requests++;
    assert.equal(url, "https://gateway.example/zstack/memos?start=10&end=19");
    assert.equal(init.credentials, "omit");
    assert.equal(init.referrerPolicy, "no-referrer");
    assert.equal(init.redirect, "error");
    assert.equal(init.priority, "low");
    assert.equal(new Headers(init.headers).has("authorization"), false);
    assert.equal(new Headers(init.headers).has("cookie"), false);
    assert.ok(init.signal instanceof AbortSignal);
    return new Response(body);
  }) as typeof fetch;
  const transport = grpcWebTransport("https://gateway.example/", { sharedMemos: true });
  assert.equal(await transport.sharedMemos!(10, 19), body);
  assert.equal(requests, 1);
});

for (const status of [403, 404, 405, 501]) test(`shared memo HTTP ${status} is cached only for that transport`, async () => {
  let requests = 0;
  globalThis.fetch = (async () => { requests++; return new Response(null, { status }); }) as typeof fetch;
  const url = "https://gateway.example";
  const transport = grpcWebTransport(url, { sharedMemos: true });
  assert.equal(await transport.sharedMemos!(1, 9), null);
  assert.equal(await transport.sharedMemos!(10, 19), null);
  assert.equal(requests, 1);
  const cancelled = AbortSignal.abort();
  await assert.rejects(transport.sharedMemos!(10, 19, cancelled), error => error === cancelled.reason);
  assert.equal(requests, 1);
  assert.equal(await grpcWebTransport(url, { sharedMemos: true }).sharedMemos!(1, 9), null);
  assert.equal(requests, 2, "a new transport can observe a newly enabled gateway");
});

for (const failure of ["network", "500", "503"] as const) test(`shared memo ${failure} failure remains retryable without a privacy downgrade`, async () => {
  const calls: string[] = [];
  globalThis.fetch = (async (url: string) => {
    calls.push(url);
    if (calls.length === 1) {
      if (failure === "network") throw new TypeError("Failed to fetch");
      return new Response(null, { status: Number(failure) });
    }
    return new Response('{"start":1,"end":9}');
  }) as typeof fetch;
  const transport = grpcWebTransport("https://gateway.example", { sharedMemos: true });
  await assert.rejects(transport.sharedMemos!(1, 9));
  assert.equal(await transport.sharedMemos!(1, 9), '{"start":1,"end":9}');
  assert.deepEqual(calls, ["https://gateway.example/zstack/memos?start=1&end=9", "https://gateway.example/zstack/memos?start=1&end=9"]);
});

test("cancelled shared memo responses cannot cache unsupported status or publish a late body", async () => {
  for (const phase of ["before", "headers", "body"] as const) {
    const caller = new AbortController();
    let requests = 0;
    globalThis.fetch = (async (_url: string, init: RequestInit) => {
      requests++;
      if (requests > 1) return new Response('{"start":1,"end":9}');
      if (phase === "headers") { caller.abort(); return new Response(null, { status: 404 }); }
      return new Response(new ReadableStream<Uint8Array>({
        pull(controller) {
          caller.abort();
          assert.equal(init.signal?.aborted, true);
          controller.enqueue(new TextEncoder().encode('{"start":1,"end":9}'));
          controller.close();
        },
      }, { highWaterMark: 0 }));
    }) as typeof fetch;
    const transport = grpcWebTransport("https://gateway.example", { sharedMemos: true });
    if (phase === "before") caller.abort();
    await assert.rejects(transport.sharedMemos!(1, 9, caller.signal), error => error === caller.signal.reason);
    assert.equal(requests, phase === "before" ? 0 : 1);
    globalThis.fetch = (async () => new Response('{"start":1,"end":9}')) as typeof fetch;
    assert.equal(await transport.sharedMemos!(1, 9), '{"start":1,"end":9}', "cancellation must not poison future attempts");
  }
});

test("shared memo response size remains bounded and an oversized response does not disable retries", async () => {
  let cancelled = false;
  globalThis.fetch = (async () => new Response(new ReadableStream<Uint8Array>({
    start(controller) {
      const chunk = new Uint8Array(8 * 1024 * 1024);
      for (let i = 0; i < 9; i++) controller.enqueue(chunk);
    },
    cancel() { cancelled = true; },
  }))) as typeof fetch;
  const transport = grpcWebTransport("https://gateway.example", { sharedMemos: true });
  await assert.rejects(transport.sharedMemos!(1, 9), /size limit/);
  assert.equal(cancelled, true);
  globalThis.fetch = (async () => new Response('{"start":1,"end":9}')) as typeof fetch;
  assert.equal(await transport.sharedMemos!(1, 9), '{"start":1,"end":9}');
});

test("a gRPC-Web body deadline remains retryable when the browser reports AbortError", async (t) => {
  const deadline = new AbortController();
  const reason = new DOMException("Response deadline exceeded", "TimeoutError");
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  globalThis.fetch = (async () => ({
    ok: true,
    headers: new Headers(),
    async arrayBuffer() {
      deadline.abort(reason);
      throw new DOMException("The user aborted a request.", "AbortError");
    },
  })) as typeof fetch;
  await assert.rejects(grpcWebTransport("https://lwd.example").blocks(1, 1000), (error) => {
    assert.equal(error, reason);
    assert.equal(isTransientLightServerError(error), true);
    return true;
  });
});

test("caller cancellation takes precedence over a gRPC-Web body deadline", async (t) => {
  const deadline = new AbortController();
  const caller = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  globalThis.fetch = (async () => ({
    ok: true,
    headers: new Headers(),
    async arrayBuffer() {
      deadline.abort(new DOMException("Response deadline exceeded", "TimeoutError"));
      caller.abort();
      throw new DOMException("The user aborted a request.", "AbortError");
    },
  })) as typeof fetch;
  await assert.rejects(grpcWebTransport("https://lwd.example").blocks(1, 1000, caller.signal), (error) => {
    assert.equal(error, caller.signal.reason);
    assert.equal(isTransientLightServerError(error), false);
    return true;
  });
});

test("every HTTP pipe route preserves explicit bearer auth and uses credentialless requests", async () => {
  const paths: string[] = [];
  globalThis.fetch = (async (input: unknown, init: RequestInit) => {
    const path = new URL(String(input)).pathname;
    paths.push(path);
    assert.equal(init.credentials, "omit", path);
    assert.equal(init.referrerPolicy, "no-referrer", path);
    assert.equal(init.redirect, "error", path);
    assert.equal(new Headers(init.headers).get("authorization"), "Bearer explicit-token", path);
    assert.equal(new Headers(init.headers).has("cookie"), false);
    if (path.endsWith("/blocks")) return new Response(new Uint8Array([0, 0, 0, 1, 7]));
    return Response.json({ tip: 10, hex: "00", height: 1, hash: "block", roots: [], txs: [], utxos: [] });
  }) as typeof fetch;
  const pipe = httpLwdTransport("http://127.0.0.1:1239", "regtest", undefined, undefined, "explicit-token");
  await pipe.tip();
  await pipe.blocks(1, 1);
  let pages = 0;
  await pipe.blockStream!(1, 1, 1, async (_blob, n) => { pages += n; });
  await pipe.info!();
  await pipe.submit!("00");
  await pipe.utxos!(["tm-fixture"]);
  await pipe.mempool!();
  await pipe.tx!("tx-fixture");
  await pipe.treeState!(1);
  await pipe.subtreeRoots!("orchard");
  await pipe.mine!(1);
  assert.equal(paths.length, 11);
  assert.equal(pages, 1, "streamed pages still reach the consumer");
  await createEngineClient("http://127.0.0.1:8787", { token: "explicit-token" }).health();
});

test("chain and native bridge requests reject redirects before another endpoint receives them", async (t) => {
  const paths: string[] = [];
  const server = createServer((req, res) => {
    paths.push(req.url!);
    if (req.url === "/destination") { res.end("redirect followed"); return; }
    res.writeHead(307, { location: "/destination" });
    res.end();
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => { server.closeAllConnections(); server.close(); });
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const base = `http://127.0.0.1:${address.port}`;
  await assert.rejects(grpcWebTransport(base).tip());
  await assert.rejects(httpLwdTransport(base, "regtest").tip());
  await assert.rejects(httpLwdTransport(base, "regtest", undefined, undefined, "explicit-token").submit!("abcd"));
  await assert.rejects(createEngineClient(base, { token: "explicit-token" }).health());
  assert.equal(paths.length, 4);
  assert.equal(paths.includes("/destination"), false);
});

test("submit sends RawTransaction and surfaces a rejection", async () => {
  const calls = mockFetch(() => reply(new Uint8Array([0x08, 0x7f, 0x12, 0x03, ...Buffer.from("bad")])));
  const t = grpcWebTransport("https://lwd.example");
  await assert.rejects(t.submit!("abcd"), /SendTransaction rejected \(127\): bad/);
  assert.equal(calls[0]!.method, "SendTransaction");
  assert.deepEqual([...calls[0]!.body], [0x0a, 0x02, 0xab, 0xcd]);
});

test("large transactions are not sent with keepalive (64 KiB cap)", async () => {
  const calls = mockFetch(() => reply(new Uint8Array([0x12, 0x02, ...Buffer.from("ok")])));
  const t = grpcWebTransport("https://lwd.example");
  assert.equal(await t.submit!("00".repeat(70_000)), "ok");
  assert.equal(calls[0]!.keepalive, false);
});

test("tx asks in wire order first, then display order", async () => {
  const txid = "01".padEnd(64, "0");
  const seen: string[] = [];
  mockFetch((_m, body) => {
    // Matches zakura-client-backend's generated TxFilter: hash is field 3.
    assert.equal(body[0], 0x1a);
    const hash = Buffer.from(body.subarray(2)).toString("hex");
    seen.push(hash);
    return seen.length === 1 ? reply() : reply(new Uint8Array([0x0a, 0x02, 0xbe, 0xef]));
  });
  const t = grpcWebTransport("https://lwd.example");
  assert.equal(await t.tx!(txid), "beef");
  assert.equal(seen[0], Buffer.from(txid, "hex").reverse().toString("hex"));
  assert.equal(seen[1], txid);
});

test("utxos are opt-in and decode 64-bit values", async () => {
  assert.equal(grpcWebTransport("https://lwd.example").utxos, undefined, "public servers stay shield-only by default");
  const value = 2_100_000_000_000_000; // 21M ZEC in zatoshis, above 2^32
  const varint = (n: number) => { const o: number[] = []; while (n >= 128) { o.push((n % 128) | 128); n = Math.floor(n / 128); } o.push(n); return o; };
  const entry = new Uint8Array([
    // GetAddressUtxosReply field numbers from lightwallet-protocol/service.proto.
    0x0a, 0x02, 0xaa, 0xbb, // txid
    0x10, 0x03, // index
    0x1a, 0x01, 0x76, // script
    0x20, ...varint(value), // valueZat
    0x28, 0x4d, // height 77
    0x32, 0x02, ...Buffer.from("tm"), // address
  ]);
  const list = new Uint8Array([0x0a, entry.length, ...entry]);
  assert.deepEqual(decodeAddressUtxos(list), [{ txid: "aabb", index: 3, script: "76", valueZat: value, height: 77, address: "tm" }]);
  const calls = mockFetch(() => reply(list));
  const t = grpcWebTransport("https://lwd.example", { transparent: true });
  assert.equal((await t.utxos!(["tmAddr"], 5)).length, 1);
  assert.deepEqual([...calls[0]!.body], [0x0a, 0x06, ...Buffer.from("tmAddr"), 0x10, 0x05]);
});

test("grpc errors carry the decoded server message", async () => {
  mockFetch(() => frame(new TextEncoder().encode("grpc-status:13\r\ngrpc-message:backing%20node%20down\r\n"), true));
  await assert.rejects(grpcWebTransport("https://lwd.example").tip(), /grpc-web status 13: backing node down/);
});

test("lightServer picks gRPC-Web for hosted URLs and the pipe for z-wallet pipe", async () => {
  const { lightServer } = await import("../src/create-wallet.ts");
  const hosted = lightServer("https://zec.rocks", { network: "mainnet" });
  assert.equal(hosted.kind, "grpc-web");
  assert.equal(hosted.utxos, undefined, "remote servers do not see transparent addresses by default");
  assert.ok(lightServer("https://zec.rocks", { network: "mainnet", transparent: true }).utxos);
  assert.ok(lightServer("http://127.0.0.1:28138", { network: "regtest" }).utxos, "loopback servers scan transparent by default");
  assert.equal(lightServer("http://127.0.0.1:1239", { network: "mainnet" }).kind, "lwd-pipe");
  assert.equal(hosted.sharedMemos, undefined);
  assert.equal(lightServer("http://127.0.0.1:28138", { network: "regtest" }).sharedMemos, undefined);
  assert.ok(lightServer("https://gateway.example", { network: "mainnet", sharedMemos: true }).sharedMemos);
  assert.equal(lightServer("https://gateway.example", { network: "mainnet", sharedMemos: false }).sharedMemos, undefined);
  assert.equal(lightServer("http://127.0.0.1:1239", { network: "regtest", sharedMemos: true }).sharedMemos, undefined, "the native pipe has no such extension");
});

test("a trailers-only error in the headers is an error, not an empty success", async () => {
  globalThis.fetch = (async () =>
    new Response(new Uint8Array(), {
      status: 200,
      headers: { "grpc-status": "14", "grpc-message": "backend%20unavailable" },
    })) as typeof fetch;
  await assert.rejects(grpcWebTransport("https://lwd.example").submit!("abcd"), /grpc-web status 14: backend unavailable/);
});

test("a SendTransaction with no response is not reported as broadcast", async () => {
  mockFetch(() => reply());
  await assert.rejects(grpcWebTransport("https://lwd.example").submit!("abcd"), /no response/);
});

test("transparent: false also keeps the local pipe from looking up addresses", async () => {
  const { lightServer } = await import("../src/create-wallet.ts");
  assert.ok(lightServer("http://127.0.0.1:1239", { network: "mainnet" }).utxos, "the pipe scans transparent by default");
  assert.equal(lightServer("http://127.0.0.1:1239", { network: "mainnet", transparent: false }).utxos, undefined);
  assert.equal(lightServer("https://lwd.example:1239", { network: "mainnet" }).utxos, undefined, "a remote pipe stays shield-only");
  assert.ok(lightServer("https://lwd.example:1239", { network: "mainnet", transparent: true }).utxos);
});

test("a SendResponse rejection code is a signed int32", async () => {
  const { protoInt32 } = await import("../src/lwd.ts");
  // protobuf int32 -26, sign-extended to a 10-byte varint on field 1.
  const negative = new Uint8Array([0x08, 0xe6, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
  assert.equal(protoInt32(negative, 1), -26);
  assert.equal(protoInt32(new Uint8Array([0x08, 0x00]), 1), 0);
});

test("a response frame without a gRPC status is not a broadcast", async () => {
  globalThis.fetch = (async () => new Response(new Uint8Array([0, 0, 0, 0, 0]), { status: 200 })) as typeof fetch;
  await assert.rejects(grpcWebTransport("https://lwd.example").submit!("abcd"), /without a gRPC status/);
});

test("a node's explicit answer to a broadcast is told apart from an unknown outcome", async () => {
  const { submitVerdict } = await import("../src/wasm-client.ts");
  const node = (code: number, reason: string) => new BroadcastRejection(code, reason);
  assert.equal(submitVerdict(node(-27, "transaction already in mempool")), "accepted");
  assert.equal(submitVerdict(node(-25, "transaction was committed to the best chain")), "accepted");
  assert.equal(submitVerdict(node(-1, "any transaction with the same effects will be rejected from the mempool until a chain reset: transaction was committed to the best chain")), "accepted");
  assert.equal(submitVerdict(node(-1, "any transaction with the same effects will be rejected from the mempool until a chain reset: expired")), "rejected");
  assert.equal(submitVerdict(node(-27, "txn-already-known")), "accepted");
  assert.equal(submitVerdict(node(-26, "bad-txns-nullifier-conflict")), "rejected");
  // "already" or "committed" alone is no duplicate: these are refusals.
  assert.equal(submitVerdict(node(-26, "nullifier already spent")), "rejected");
  assert.equal(submitVerdict(node(-26, "nullifier already known")), "rejected");
  assert.equal(submitVerdict(node(-26, "conflicts with a transaction already in the mempool")), "rejected");
  assert.equal(submitVerdict(node(-25, "anchor is not committed")), "rejected");
  // Transport failures stay unknown: the send is kept and resent. So does
  // text that merely looks like a node reply.
  assert.equal(submitVerdict(new Error("grpc-web status 14: backend unavailable")), null);
  assert.equal(submitVerdict(new Error("SendTransaction ended without a gRPC status")), null);
  assert.equal(submitVerdict(new Error("SendTransaction rejected (-26): bad-txns-nullifier-conflict")), null);
  assert.equal(submitVerdict(new TypeError("fetch failed")), null);
});

test("gRPC-Web and the loopback bridge both report the node's refusal as a BroadcastRejection", async () => {
  mockFetch(() => reply(new Uint8Array([0x08, 0x1a, 0x12, 0x03, ...Buffer.from("bad")])));
  const grpc = await grpcWebTransport("https://lwd.example").submit!("abcd").catch((e: unknown) => e);
  assert.ok(grpc instanceof BroadcastRejection && grpc.code === 26 && grpc.reason === "bad");

  const bridge = (status: number, body: unknown) => {
    globalThis.fetch = (async () => new Response(JSON.stringify(body), { status })) as typeof fetch;
    return httpLwdTransport("http://127.0.0.1:8787", "regtest").submit!("abcd").catch((e: unknown) => e);
  };
  const structured = await bridge(500, {
    error: "broadcast rejected (-26): nullifier already spent",
    rejected: { code: -26, message: "nullifier already spent" },
  });
  assert.ok(structured instanceof BroadcastRejection && structured.reason === "nullifier already spent");
  // A bridge from before the structured field: the engine's error text.
  const older = await bridge(500, { error: "broadcast rejected (-27): transaction already in mempool" });
  assert.ok(older instanceof BroadcastRejection && older.code === -27);
  // Anything else is an unknown outcome.
  const unknown = await bridge(500, { error: "zebra rpc read: connection reset" });
  assert.ok(unknown instanceof Error && !(unknown instanceof BroadcastRejection));
});

test("capability negotiation reads multibyte protobuf tags before requesting all pools", async () => {
  const calls = mockFetch((method, body) => {
    if (method === "GetLightdInfo") return reply(new Uint8Array([
      0x32, 8, ...new TextEncoder().encode("77190ad9"), // field 6, NU7 branch
      0x88, 0x01, 0x96, 0x01, // field 17, upgrade height 150
      0x92, 0x01, 6, ...new TextEncoder().encode("v0.5.0"), // field 18
    ]));
    assert.equal(method, "GetBlockRange");
    assert.deepEqual([...body.subarray(-6)], [0x1a, 4, 1, 2, 3, 4]);
    return reply(new Uint8Array([16, 1]));
  });
  const transport = grpcWebTransport("https://lwd.example");
  const info = await transport.info!();
  assert.equal(info.protocolVersion, "v0.5.0");
  assert.equal(info.consensusBranchId, "77190ad9");
  assert.equal((await transport.transparentBlocks!(1, 1)).length, 6);
  assert.deepEqual(calls.map(x => x.method), ["GetLightdInfo", "GetBlockRange"]);
});
