// Public block-range memo bundles. Transport/protobuf only; Rust verifies txids
// and decrypts. No wallet IDs, address lists, cookies or caller headers enter RPCs.
import http2 from "node:http2";
const service = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/";
const LIMIT = 32 * 1024 * 1024;

export function varint(n) {
  const out = [];
  do { const b = n % 128; n = Math.floor(n / 128); out.push(b | (n ? 128 : 0)); } while (n);
  return Buffer.from(out);
}
export function field(tag, value) {
  return Buffer.concat([varint(tag * 8 + 2), varint(value.length), value]);
}
export function fields(bytes) {
  let offset = 0;
  const read = () => {
    let value = 0, factor = 1;
    for (let i = 0; i < 10 && offset < bytes.length; i++) {
      const b = bytes[offset++]; value += (b & 127) * factor;
      if (!Number.isSafeInteger(value)) throw new Error("protobuf overflow");
      if (!(b & 128)) return value;
      factor *= 128;
    }
    throw new Error("invalid protobuf varint");
  };
  const out = [];
  while (offset < bytes.length) {
    const key = read(), tag = Math.floor(key / 8), wire = key % 8;
    if (!tag) throw new Error("invalid protobuf field");
    if (wire === 0) out.push([tag, read()]);
    else {
      const length = wire === 2 ? read() : wire === 1 ? 8 : wire === 5 ? 4 : -1;
      if (length < 0 || length > bytes.length - offset) throw new Error("invalid protobuf length");
      out.push([tag, bytes.subarray(offset, offset + length)]); offset += length;
    }
  }
  return out;
}
const value = (list, tag) => list.find(([id]) => id === tag)?.[1];
export function grpcFrame(bytes) {
  const head = Buffer.alloc(5); head.writeUInt32BE(bytes.length, 1);
  return Buffer.concat([head, bytes]);
}

function rpc(client, method, body, signal, timeoutMs) {
  return new Promise((resolve, reject) => {
    signal.throwIfAborted();
    const stream = client().request({ ":method": "POST", ":path": `${service}${method}`,
      "content-type": "application/grpc", te: "trailers" });
    const chunks = []; let size = 0, status, valid = false, settled = false;
    const finish = (error, result) => {
      if (settled) return; settled = true;
      clearTimeout(timer); signal.removeEventListener("abort", abort);
      if (error) { stream.close(http2.constants.NGHTTP2_CANCEL); reject(error); }
      else resolve(result);
    };
    const abort = () => finish(new Error("cancelled"));
    const timer = setTimeout(() => finish(new Error("upstream timeout")), timeoutMs);
    timer.unref(); signal.addEventListener("abort", abort, { once: true });
    stream.on("response", (headers) => {
      valid = headers[":status"] === 200 && String(headers["content-type"] ?? "").startsWith("application/grpc");
      status = headers["grpc-status"];
      if (!valid) finish(new Error("upstream response"));
    });
    stream.on("trailers", (headers) => { status = headers["grpc-status"]; });
    stream.on("data", (part) => {
      size += part.length;
      if (size > LIMIT) finish(new Error("range too large"));
      else chunks.push(part);
    });
    stream.on("error", () => finish(new Error("upstream unavailable")));
    stream.on("close", () => {
      if (settled) return;
      try {
        if (!valid || String(status) !== "0") throw new Error("upstream RPC failed");
        const bytes = Buffer.concat(chunks), messages = [];
        for (let pos = 0; pos < bytes.length;) {
          if (pos + 5 > bytes.length || bytes[pos] !== 0) throw new Error("invalid gRPC frame");
          const length = bytes.readUInt32BE(pos + 1); pos += 5;
          if (length > bytes.length - pos) throw new Error("truncated gRPC frame");
          messages.push(bytes.subarray(pos, pos + length)); pos += length;
        }
        finish(null, messages);
      } catch (error) { finish(error); }
    });
    stream.end(grpcFrame(body));
  });
}

/** Bounded public cache; keys contain heights only. Recent bundles expire quickly
 * for reorgs. No per-wallet state or access log is kept by this module. */
export function sharedMemoHandler(client, timeoutMs) {
  const cache = new Map(); let cacheBytes = 0, active = 0;
  return async (req, res, cors) => {
    if (!req.url?.startsWith("/zstack/memos")) return false;
    const url = new URL(req.url, "http://localhost");
    if (url.pathname !== "/zstack/memos") return false;
    const headers = { ...cors, "content-type": "application/json", "cache-control": "no-store",
      "referrer-policy": "no-referrer" };
    if (req.method === "OPTIONS") { res.writeHead(204, cors).end(); return true; }
    if (req.method !== "GET") { res.writeHead(405, headers).end(); return true; }
    const start = Number(url.searchParams.get("start")), end = Number(url.searchParams.get("end"));
    const keys = [...url.searchParams.keys()];
    if (keys.length !== 2 || !keys.includes("start") || !keys.includes("end") ||
        !Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 1 || end < start ||
        end > 0xffffffff || end - start >= 10 || !/^\d+$/.test(url.searchParams.get("start")) ||
        !/^\d+$/.test(url.searchParams.get("end"))) {
      res.writeHead(400, headers).end('{"error":"invalid public range"}'); return true;
    }
    const key = `${start}:${end}`, now = Date.now();
    for (const [id, item] of cache) if (item.expires <= now) { cache.delete(id); cacheBytes -= item.body.length; }
    const hit = cache.get(key);
    if (hit) { res.writeHead(200, headers).end(hit.body); return true; }
    if (active >= 4) { res.writeHead(503, headers).end('{"error":"shared range busy"}'); return true; }
    active++;
    const controller = new AbortController();
    const abort = () => { if (!res.writableFinished) controller.abort(); };
    res.on("close", abort);
    const deadline = setTimeout(() => controller.abort(), timeoutMs); deadline.unref();
    try {
      const id = (height) => Buffer.concat([Buffer.from([8]), varint(height)]);
      const blocks = await rpc(client, "GetBlockRange", Buffer.concat([field(1, id(start)), field(2, id(end))]), controller.signal, timeoutMs);
      if (blocks.length !== end - start + 1) throw new Error("incomplete range");
      const transactions = [], delimited = [];
      let bytes = 0, previous;
      for (let i = 0; i < blocks.length; i++) {
        const b = fields(blocks[i]), hash = value(b, 3), prev = value(b, 4);
        if (value(b, 2) !== start + i || !Buffer.isBuffer(hash) || hash.length !== 32 ||
            !Buffer.isBuffer(prev) || prev.length !== 32 || (previous && !prev.equals(previous))) throw new Error("invalid range");
        previous = hash;
        const head = Buffer.alloc(4); head.writeUInt32BE(blocks[i].length);
        delimited.push(head, blocks[i]); bytes += blocks[i].length + 4;
        const seen = new Set();
        for (const [tag, encoded] of b) {
          if (tag !== 7) continue;
          const tx = fields(encoded);
          if (!tx.some(([id]) => [4, 5, 6, 9].includes(id))) continue;
          const txid = value(tx, 2);
          if (!Buffer.isBuffer(txid) || txid.length !== 32 || seen.has(txid.toString("hex"))) throw new Error("invalid transaction");
          seen.add(txid.toString("hex"));
          // TxFilter: block=1, index=2 (varint), hash=3 (bytes), as in
          // zakura-client-backend/src/proto/service.rs and lightwallet-protocol.
          const raw = await rpc(client, "GetTransaction", field(3, txid), controller.signal, timeoutMs);
          if (raw.length !== 1) throw new Error("missing transaction");
          const data = value(fields(raw[0]), 1);
          if (!Buffer.isBuffer(data) || !data.length) throw new Error("missing transaction");
          bytes += data.length;
          if (bytes > LIMIT - 1024 * 1024) throw new Error("range too large");
          transactions.push(data.toString("hex"));
        }
      }
      const body = Buffer.from(JSON.stringify({ start, end, blocks: Buffer.concat(delimited).toString("hex"), transactions }));
      while (cacheBytes + body.length > 64 * 1024 * 1024 && cache.size) {
        const oldest = cache.keys().next().value; cacheBytes -= cache.get(oldest).body.length; cache.delete(oldest);
      }
      cache.set(key, { body, expires: Date.now() + 10_000 }); cacheBytes += body.length;
      if (!res.destroyed) res.writeHead(200, headers).end(body);
    } catch {
      if (!res.destroyed) res.writeHead(502, headers).end('{"error":"shared range unavailable"}');
    } finally {
      clearTimeout(deadline); res.removeListener("close", abort); active--;
    }
    return true;
  };
}
