#!/usr/bin/env node
// Public block comparison only. No wallet state or address queries.
import { grpcWebTransport } from '../packages/sdk/dist/index.js';
const args = process.argv.slice(2);
let server = 'https://zcash-testnet.chainsafe.dev', explorer = 'https://zexplorer.app', height;
try {
  for (let i = 0; i < args.length; i++) {
    if (!['--server', '--explorer', '--height'].includes(args[i]) || !args[i + 1]) throw new Error();
    const flag = args[i], value = args[++i];
    if (flag === '--server') server = value;
    else if (flag === '--explorer') explorer = value;
    else height = Number(value);
  }
  if (!Number.isSafeInteger(height) || height < 1 || height > 0xffff_ffff) throw new Error();
  for (const value of [server, explorer]) {
    const url = new URL(value);
    if (url.protocol !== 'https:' || url.username || url.password || url.search || url.hash) throw new Error();
  }
} catch {
  console.error('Usage: node scripts/check-testnet-chain.mjs --height <positive block height> [--server <HTTPS gRPC-Web URL>] [--explorer <HTTPS Zexplorer origin>]');
  process.exit(2);
}
try {
  const transport = grpcWebTransport(server);
  const [state, response] = await Promise.all([
    transport.treeState(height, AbortSignal.timeout(20_000)),
    fetch(new URL(`/api/v1/testnet/blocks/${height}`, explorer), { signal: AbortSignal.timeout(20_000), credentials: 'omit', redirect: 'error' }),
  ]);
  if (!response.ok) {
    throw new Error(response.status === 404
      ? `Explorer has no block at height ${height}. Compare a height both sides publish; a missing tip often means a different chain.`
      : `Explorer request failed (${response.status}).`);
  }
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
  else if (!match) process.exitCode = 1;
} catch (error) {
  const message = error instanceof Error ? error.message : "";
  console.error(message.startsWith("Explorer ")
    ? message
    : "Could not compare public block data. Check the height, endpoint availability, and gRPC-Web support.");
  process.exitCode = 2;
}
