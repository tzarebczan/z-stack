import { regtestLwdUrl } from "./regtest-rpc.mjs";
import assert from 'node:assert/strict';
import { createGrpcWebProxy } from './grpc-web-proxy.mjs';
import { assertLocalRegtestChain } from './sdk-harness.mjs';
import { zebraRpc, ZEBRA_RPC } from './regtest-rpc.mjs';

/** Fixture-only validator fallback. Never bind remotely or use a public chain. */
export async function regtestBrowserGateway(origins, port = 0) {
  assert.ok(origins.length && origins.every(value => ['127.0.0.1', 'localhost'].includes(new URL(value).hostname)), 'Fixture origins must be loopback');
  assertLocalRegtestChain(await zebraRpc('getblockchaininfo'), ZEBRA_RPC);
  const upstream = regtestLwdUrl();
  assert.ok(['127.0.0.1', 'localhost'].includes(new URL(upstream).hostname), 'Fixture upstream must be loopback');
  let loseAcknowledgement = false;
  const server = createGrpcWebProxy({ upstream, origins, allowTransparent: true });
  const forward = server.listeners('request')[0]; server.removeAllListeners('request');
  server.on('request', async (req, res) => {
    if (!req.url?.endsWith('/SendTransaction')) { forward(req, res); return; }
    const origin = req.headers.origin;
    if (!origin || !origins.includes(origin)) { res.writeHead(403).end(); return; }
    const headers = { 'access-control-allow-origin': origin, vary: 'Origin',
      'access-control-allow-methods': 'POST, OPTIONS',
      'access-control-allow-headers': 'content-type, x-grpc-web, x-user-agent, grpc-timeout',
      'content-type': 'application/grpc-web+proto', 'cache-control': 'no-store' };
    if (req.method === 'OPTIONS') { res.writeHead(204, headers).end(); return; }
    if (req.method !== 'POST') { res.writeHead(405, headers).end(); return; }
    try {
      const chunks = []; let size = 0;
      for await (const chunk of req) { size += chunk.length; assert.ok(size <= 4_000_000); chunks.push(chunk); }
      const frame = Buffer.concat(chunks);
      assert.ok(frame.length >= 7 && frame[0] === 0 && frame.readUInt32BE(1) === frame.length - 5 && frame[5] === 10);
      let offset = 6, length = 0, shift = 0;
      for (;;) { assert.ok(offset < frame.length && shift < 28); const byte = frame[offset++]; length += (byte & 127) * 2 ** shift; if (!(byte & 128)) break; shift += 7; }
      assert.ok(length > 0 && offset + length <= frame.length);
      assertLocalRegtestChain(await zebraRpc('getblockchaininfo'), ZEBRA_RPC);
      const txid = await zebraRpc('sendrawtransaction', [frame.subarray(offset, offset + length).toString('hex')]);
      assert.match(txid, /^[a-f0-9]{64}$/i);
      if (loseAcknowledgement) { loseAcknowledgement = false; res.writeHead(502, headers).end(); return; }
      const message = Buffer.concat([Buffer.from([18, 64]), Buffer.from(txid)]);
      const frameHeader = Buffer.alloc(5); frameHeader.writeUInt32BE(message.length, 1);
      const trailers = Buffer.from('grpc-status:0\r\n');
      const trailerHeader = Buffer.alloc(5); trailerHeader[0] = 128; trailerHeader.writeUInt32BE(trailers.length, 1);
      res.writeHead(200, headers).end(Buffer.concat([frameHeader, message, trailerHeader, trailers]));
    } catch { res.writeHead(502, headers).end(); }
  });
  await new Promise(resolve => server.listen(port, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${server.address().port}`, loseNextAcknowledgement: () => { loseAcknowledgement = true; }, stop: () => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }) };
}
