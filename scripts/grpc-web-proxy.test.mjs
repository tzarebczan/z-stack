import assert from "node:assert/strict";
import { once } from "node:events";
import http2 from "node:http2";
import test from "node:test";
import { createGrpcWebProxy } from "./grpc-web-proxy.mjs";
import { proxyPolicy } from "./grpc-web-proxy-policy.mjs";

const service = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/";
const frame = Buffer.from([0, 0, 0, 0, 1, 42]);
const post = (headers = {}) => ({ method: "POST", body: frame,
  headers: { "content-type": "application/grpc-web+proto", ...headers } });
async function listen(server) {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return `http://127.0.0.1:${server.address().port}`;
}
async function fixture(t, handler, options = {}) {
  const upstream = http2.createServer();
  const sessions = new Set();
  upstream.on("session", (session) => {
    sessions.add(session);
    session.on("close", () => sessions.delete(session));
  });
  upstream.on("stream", (stream, headers) => {
    stream.on("error", () => {});
    handler(stream, headers);
  });
  const address = await listen(upstream);
  const proxy = createGrpcWebProxy({ upstream: address,
    origins: ["http://localhost:5180"], ...options });
  const url = await listen(proxy);
  t.after(async () => {
    proxy.closeAllConnections();
    await new Promise((resolve) => proxy.close(resolve));
    for (const session of sessions) session.destroy();
    await new Promise((resolve) => upstream.close(resolve));
  });
  return `${url}${service}`;
}

test("a local adapter cannot silently forward transparent addresses to a remote server", () => {
  const remote = proxyPolicy("https://zaino.example");
  assert.equal(remote.allows(`${service}GetTransaction`), true);
  assert.equal(remote.allows(`${service}GetAddressUtxos`), false);
  assert.equal(remote.allows(`${service}GetTaddressTxids`), false);
  assert.equal(proxyPolicy("https://zaino.example", true).allows(`${service}GetAddressUtxos`), true);
  assert.equal(proxyPolicy("http://127.0.0.1:28137").allows(`${service}GetAddressUtxos`), true);
  for (const path of ["/admin", `${service}GetTransaction?address=secret`, `${service}../GetTransaction`, `${service}Unknown`])
    assert.equal(remote.allows(path), false);
  for (const origin of ["https://user:secret@zaino.example", "https://zaino.example/api", "https://zaino.example/?key=secret"])
    assert.throws(() => proxyPolicy(origin), /without credentials/);
});

test("forwards gRPC data and trailers without client identity headers", { timeout: 5000 }, async (t) => {
  let received;
  const base = await fixture(t, (stream, headers) => {
    received = headers;
    stream.respond({ ":status": 200, "content-type": "application/grpc" }, { waitForTrailers: true });
    stream.on("wantTrailers", () => stream.sendTrailers({ "grpc-status": "0" }));
    stream.end(frame);
  });
  const response = await fetch(`${base}GetLatestBlock`, post({
    origin: "http://localhost:5180", authorization: "Bearer secret", cookie: "wallet=secret",
    referer: "https://wallet.example/private", "x-forwarded-for": "192.0.2.4",
    "cf-connecting-ip": "192.0.2.4", "x-wallet-fingerprint": "private-wallet",
  }));
  const body = Buffer.from(await response.arrayBuffer());
  assert.deepEqual(body.subarray(0, frame.length), frame);
  assert.match(body.subarray(frame.length).toString(), /grpc-status:0/);
  for (const name of ["authorization", "cookie", "referer", "origin", "x-forwarded-for", "cf-connecting-ip", "x-wallet-fingerprint"])
    assert.equal(received[name], undefined);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(response.headers.get("access-control-allow-origin"), "http://localhost:5180");
});

test("forbidden methods, origins, oversize requests and text mode never reach upstream", { timeout: 5000 }, async (t) => {
  let calls = 0;
  const base = await fixture(t, () => { calls++; }, { maxRequestBytes: 8, allowTransparent: false });
  assert.equal((await fetch(`${base}GetAddressUtxos`, post())).status, 403);
  assert.equal((await fetch(`${base}Unknown`, post())).status, 403);
  assert.equal((await fetch(`${base}GetTransaction?txid=secret`, post())).status, 403);
  assert.equal((await fetch(`${base}GetTransaction`, post({ origin: "https://untrusted.example" }))).status, 403);
  assert.equal((await fetch(`${base}GetTransaction`, post({ "content-type": "application/grpc-web-text" }))).status, 415);
  assert.equal((await fetch(`${base}GetTransaction`, { ...post(), body: Buffer.alloc(9) })).status, 413);
  assert.equal(calls, 0);
});

test("streams the first frame before the upstream finishes and cancels on disconnect", { timeout: 5000 }, async (t) => {
  let ended = false;
  let closed;
  const closedPromise = new Promise((resolve) => { closed = resolve; });
  const base = await fixture(t, (stream) => {
    stream.on("close", () => { ended = true; closed(); });
    stream.respond({ ":status": 200, "content-type": "application/grpc" });
    stream.write(frame); // Deliberately never finish: buffering would hang.
  });
  const abort = new AbortController();
  const response = await fetch(`${base}GetBlockRange`, { ...post(), signal: abort.signal });
  const reader = response.body.getReader();
  assert.deepEqual(Buffer.from((await reader.read()).value), frame);
  assert.equal(ended, false);
  abort.abort();
  await closedPromise;
});

test("bounds a stalled upstream and strips request data from upstream errors", { timeout: 5000 }, async (t) => {
  const base = await fixture(t, (stream, headers) => {
    if (headers[":path"].endsWith("GetLatestBlock")) return;
    stream.respond({ ":status": 200, "content-type": "application/grpc",
      "grpc-status": "5", "grpc-message": "txid=private-request-and-wallet-address" });
    stream.end();
  }, { timeoutMs: 50 });
  const failed = await (await fetch(`${base}GetTransaction`, post())).text();
  assert.match(failed, /grpc-status:5/);
  assert.doesNotMatch(failed, /private-request|wallet-address/);
  const timeout = await (await fetch(`${base}GetLatestBlock`, post())).text();
  assert.match(timeout, /grpc-status:4/);
});

// Public retrieval fans out on every shielded tx in a range, never a supplied
// wallet list. The engine separately verifies raw transaction identifiers.
test("shared memos use public ranges, include unrelated txs, cache ranges and strip identity", async (t) => {
  const { field, varint, grpcFrame, fields } = await import("./shared-memos.mjs");
  const txids = [Buffer.alloc(32, 3), Buffer.alloc(32, 4)];
  const compact = Buffer.concat([Buffer.from([16, 1]), field(3, Buffer.alloc(32, 1)), field(4, Buffer.alloc(32)),
    ...txids.map((id, i) => field(7, Buffer.concat([Buffer.from([8, i + 1]), field(2, id), field(6, Buffer.from([1]))])))]);
  const calls = [];
  const base = await fixture(t, (stream, headers) => {
    const body = [];
    stream.on("data", (chunk) => body.push(chunk));
    stream.on("end", () => {
      calls.push(headers[":path"].split("/").pop());
      for (const name of ["authorization", "cookie", "referer", "x-forwarded-for"]) assert.equal(headers[name], undefined);
      const request = Buffer.concat(body).subarray(5);
      let response = compact;
      if (calls.at(-1) === "GetTransaction") {
        const hash = fields(request).find(([id]) => id === 3)?.[1];
        assert.ok(txids.some((id) => id.equals(hash)));
        response = field(1, Buffer.from([hash[0], 42]));
      }
      stream.respond({ ":status": 200, "content-type": "application/grpc" }, { waitForTrailers: true });
      stream.on("wantTrailers", () => stream.sendTrailers({ "grpc-status": "0" }));
      stream.end(grpcFrame(response));
    });
  });
  const url = `${new URL(base).origin}/zstack/memos?start=1&end=1`;
  const response = await fetch(url, { headers: { cookie: "private", authorization: "private", "x-forwarded-for": "private" } });
  assert.equal(response.status, 200);
  const bundle = await response.json();
  assert.deepEqual(bundle.transactions, ["032a", "042a"]);
  assert.equal(bundle.start, 1);
  assert.deepEqual(calls, ["GetBlockRange", "GetTransaction", "GetTransaction"]);
  assert.equal((await fetch(url)).status, 200);
  assert.equal(calls.length, 3);
  for (const query of ["start=1&end=11", "start=1&end=1&txid=private", "start=1&end=1&start=2", "start=NaN&end=1"])
    assert.equal((await fetch(`${new URL(base).origin}/zstack/memos?${query}`)).status, 400);
  assert.equal(calls.length, 3);
});

test("shared memos reject incomplete upstream blocks and sanitize errors", async (t) => {
  const base = await fixture(t, (stream) => {
    stream.respond({ ":status": 200, "content-type": "application/grpc" }, { waitForTrailers: true });
    stream.on("wantTrailers", () => stream.sendTrailers({ "grpc-status": "0" }));
    stream.end();
  });
  const response = await fetch(`${new URL(base).origin}/zstack/memos?start=1&end=2`);
  assert.equal(response.status, 502);
  assert.deepEqual(await response.json(), { error: "shared range unavailable" });
});
