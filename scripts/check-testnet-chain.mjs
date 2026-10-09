#!/usr/bin/env node
// Public block comparison only. No wallet state or address queries.
import { existsSync } from 'node:fs';
const sourceSdk = new URL('../packages/sdk/dist/index.js', import.meta.url);
let grpcWebTransport;
try {
  ({ grpcWebTransport } = await import(existsSync(sourceSdk) ? sourceSdk.href : '@z-stack/sdk'));
} catch {
  console.error('Run this from an installed generated wallet (npm run check:chain), or build the source SDK with pnpm build:sdk.');
  process.exit(2);
}
const args = process.argv.slice(2);
let server = 'https://zcash-testnet.chainsafe.dev', explorer = 'https://zexplorer.app', height, expectedHash;
class PublicLookupError extends Error {}
try {
  for (let i = 0; i < args.length; i++) {
    if (!['--server', '--explorer', '--height', '--expected-hash'].includes(args[i]) || !args[i + 1]) throw new Error();
    const flag = args[i], value = args[++i];
    if (flag === '--server') server = value;
    else if (flag === '--explorer') explorer = value;
    else if (flag === '--expected-hash') expectedHash = value.toLowerCase();
    else height = Number(value);
  }
  if (!Number.isSafeInteger(height) || height < 1 || height > 0xffff_ffff) throw new Error();
  if (expectedHash !== undefined && !/^[0-9a-f]{64}$/.test(expectedHash)) throw new Error();
  for (const value of [server, explorer]) {
    const url = new URL(value);
    if (url.protocol !== 'https:' || url.username || url.password || url.search || url.hash) throw new Error();
  }
} catch {
  console.error('Usage: node scripts/check-testnet-chain.mjs --height <positive block height> [--server <HTTPS gRPC-Web URL>] [--explorer <HTTPS Zexplorer origin>] [--expected-hash <64 hex characters>]');
  process.exit(2);
}
try {
  const transport = grpcWebTransport(server);
  if (expectedHash) {
    const state = await transport.treeState(height, AbortSignal.timeout(20_000));
    if ((state.network !== "test" && state.network !== "testnet") || state.height !== height || !/^[0-9a-f]{64}$/i.test(state.hash)) throw new Error();
    const match = state.hash.toLowerCase() === expectedHash;
    console.log(JSON.stringify({ height, server: new URL(server).host, serverHash: state.hash, expectedHash, match }, null, 2));
    console.log('This checks one public block, not faucet funding or full consensus validation.');
    process.exitCode = match ? 0 : 1;
  } else {
  const [lookup, page] = await Promise.allSettled([
    transport.treeState(height, AbortSignal.timeout(20_000)),
    fetch(new URL(`/api/v1/testnet/blocks/${height}`, explorer), { signal: AbortSignal.timeout(20_000), credentials: 'omit', redirect: 'error' }),
  ]);
  if (page.status === 'rejected') throw new PublicLookupError(`Explorer lookup failed at height ${height}. Check explorer availability.`);
  const response = page.value;
  if (response.status === 404) throw new PublicLookupError(`Explorer has no block at height ${height} (HTTP 404). Compare a height available on both providers; a missing block alone does not prove a fork.`);
  if (!response.ok) throw new PublicLookupError(`Explorer lookup failed at height ${height} (HTTP ${response.status}).`);
  if (lookup.status === 'rejected') throw new PublicLookupError(`Light-server lookup failed at height ${height}. Check endpoint availability, the server tip and gRPC-Web support.`);
  const state = lookup.value;
  const result = await response.json();
  const block = result?.data?.summary;
  const valid = hash => typeof hash === 'string' && /^[0-9a-f]{64}$/i.test(hash);
  if (state.network !== "test" && state.network !== "testnet") throw new Error();
  if (state.height !== height || block?.block_height !== height || result?.freshness?.network !== "testnet" || block?.is_canonical !== true || !valid(state.hash) || !valid(block.block_hash)) throw new Error();
  const reverse = hash => hash.match(/../g).reverse().join("").toLowerCase();
  const match = state.hash.toLowerCase() === block.block_hash.toLowerCase();
  const reversedMatch = !match && reverse(state.hash) === block.block_hash.toLowerCase();
  console.log(JSON.stringify({ height, server: new URL(server).host, explorer: new URL(explorer).host,
    serverHash: state.hash, explorerHash: block.block_hash, match, reversedMatch }, null, 2));
  if (reversedMatch) { console.error("Hashes match in reversed byte order. Normalize provider encoding before diagnosing a fork."); process.exitCode = 2; }
  else if (!match) { console.error("Providers disagree at this height. This does not by itself prove a faucet or wallet cannot send."); process.exitCode = 1; }
  }
} catch (error) {
  console.error(error instanceof PublicLookupError ? error.message : 'Could not compare public block data. Check the height, endpoint availability, and gRPC-Web support.');
  process.exitCode = 2;
}
