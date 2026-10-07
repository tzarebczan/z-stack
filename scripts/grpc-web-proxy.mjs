#!/usr/bin/env node
// Loopback gRPC-Web front for native Zaino/lightwalletd. This is a local
// development adapter, not a production privacy gateway or an attested TEE.
//   node scripts/grpc-web-proxy.mjs --upstream http://127.0.0.1:28137 --port 28138
// A remote upstream refuses transparent lookups unless --allow-transparent is
// supplied: the local browser URL must not silently expose addresses remotely.
import http from "node:http";
import http2 from "node:http2";
import { pathToFileURL } from "node:url";
import { sharedMemoHandler } from "./shared-memos.mjs";
import { proxyPolicy } from "./grpc-web-proxy-policy.mjs";

function trailerFrame(status, message = "") {
  const text = Buffer.from(`grpc-status:${status}\r\ngrpc-message:${encodeURIComponent(message)}\r\n`);
  const head = Buffer.alloc(5);
  head[0] = 0x80;
  head.writeUInt32BE(text.length, 1);
  return Buffer.concat([head, text]);
}

export function createGrpcWebProxy({ upstream, origins = [], allowTransparent,
  maxRequestBytes = 4 * 1024 * 1024, timeoutMs = 180_000 }) {
  const policy = proxyPolicy(upstream, allowTransparent);
  let session;
  function client() {
    if (session && !session.closed && !session.destroyed) return session;
    const next = http2.connect(policy.origin);
    session = next;
    next.on("error", () => { if (session === next) session = undefined; });
    return next;
  }

  const sharedMemos = sharedMemoHandler(client, timeoutMs);
  const server = http.createServer(async (req, res) => {
    const origin = req.headers.origin;
    const cors = origin && origins.includes(origin) ? {
      "access-control-allow-origin": origin,
      vary: "Origin",
      "access-control-allow-methods": "GET, POST, OPTIONS",
      "access-control-allow-headers": "content-type, x-grpc-web, x-user-agent, grpc-timeout",
      "access-control-expose-headers": "grpc-status, grpc-message",
      "access-control-max-age": "600",
    } : {};
    if (origin && !origins.includes(origin)) return res.writeHead(403).end();
    if (await sharedMemos(req, res, cors)) return;
    if (!policy.allows(req.url)) return res.writeHead(403, cors).end("method not allowed");
    if (req.method === "OPTIONS") return res.writeHead(204, cors).end();
    if (req.method !== "POST") return res.writeHead(405, cors).end();
    const contentType = req.headers["content-type"]?.split(";", 1)[0].trim();
    if (!["application/grpc-web+proto", "application/grpc-web"].includes(contentType))
      return res.writeHead(415, cors).end();

    const chunks = [];
    let size = 0;
    let stopped = false;
    req.on("error", () => { stopped = true; res.destroy(); });
    req.on("aborted", () => { stopped = true; });
    req.on("data", (chunk) => {
      if (stopped) return;
      size += chunk.length;
      if (size > maxRequestBytes) {
        stopped = true;
        chunks.length = 0;
        res.writeHead(413, { ...cors, connection: "close" }).end();
      } else chunks.push(chunk);
    });
    req.on("end", () => {
      if (stopped || res.destroyed) return;
      let stream;
      let done = false;
      let headers = {};
      let trailers = {};
      let timer;
      const begin = () => {
        if (!res.headersSent) res.writeHead(200, {
          ...cors, "content-type": "application/grpc-web+proto",
          "cache-control": "no-store", "referrer-policy": "no-referrer",
        });
      };
      const finish = (status, message = "") => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        if (!res.destroyed) { begin(); res.end(trailerFrame(status, message)); }
      };
      try {
        // Never copy Cookie, Authorization, Origin, Referer, forwarding headers,
        // or account identifiers onto a chain request.
        stream = client().request({
          ":method": "POST", ":path": req.url,
          "content-type": "application/grpc", te: "trailers",
        });
      } catch {
        finish(14, "upstream unavailable");
        return;
      }
      timer = setTimeout(() => {
        finish(4, "upstream timeout");
        stream.close(http2.constants.NGHTTP2_CANCEL);
      }, timeoutMs);
      timer.unref();
      stream.on("response", (value) => {
        headers = value;
        if (value[":status"] !== 200 || !String(value["content-type"] ?? "").startsWith("application/grpc")) {
          finish(14, "invalid upstream response");
          stream.close(http2.constants.NGHTTP2_CANCEL);
        }
      });
      stream.on("data", (chunk) => {
        if (done) return;
        begin();
        if (!res.write(chunk)) stream.pause();
      });
      res.on("drain", () => { if (!done) stream.resume(); });
      stream.on("trailers", (value) => { trailers = value; });
      stream.on("close", () => {
        const status = String(trailers["grpc-status"] ?? headers["grpc-status"] ?? "14");
        // Upstream descriptions can echo address, txid, or request data.
        finish(/^(?:[0-9]|1[0-6])$/.test(status) ? status : "14",
          status === "0" ? "" : "upstream RPC failed");
      });
      stream.on("error", () => finish(14, "upstream unavailable"));
      res.on("close", () => {
        if (res.writableFinished) return;
        done = true;
        clearTimeout(timer);
        stream.close(http2.constants.NGHTTP2_CANCEL);
      });
      stream.end(Buffer.concat(chunks));
      chunks.length = 0;
    });
  });
  server.on("close", () => session?.destroy());
  return server;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const arg = (name, fallback) => {
    const i = process.argv.indexOf(name);
    return i > 0 ? process.argv[i + 1] : fallback;
  };
  const upstream = arg("--upstream", process.env.GRPC_UPSTREAM ?? "http://127.0.0.1:28137");
  const port = Number(arg("--port", process.env.GRPC_WEB_PORT ?? "28138"));
  const origins = arg("--origins", process.env.GRPC_WEB_ORIGINS ??
    "http://localhost:5173,http://127.0.0.1:5173,http://localhost:5174,http://127.0.0.1:5174")
    .split(",").map((value) => value.trim()).filter(Boolean);
  createGrpcWebProxy({ upstream, origins,
    allowTransparent: process.argv.includes("--allow-transparent") ? true : undefined,
  }).listen(port, "127.0.0.1", () => {
    console.log(`Local gRPC-Web adapter listening on 127.0.0.1:${port}`);
  });
}
