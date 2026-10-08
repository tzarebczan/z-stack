#!/usr/bin/env node
import { regtestLwdUrl } from "./regtest-rpc.mjs";
// End-to-end Rust/WASM + real Zaino acceptance. Only a loopback regtest;
// built-in public test mnemonic. Addresses are forbidden; shared mode also
// forbids owned-transaction queries. Z_STACK_MEMO_FETCH=auto tests selective recovery.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { once } from "node:events";
import { createGrpcWebProxy } from "./grpc-web-proxy.mjs";
import { assertLocalRegtestChain } from "./sdk-harness.mjs";
import { generate, zebraRpc, ZEBRA_RPC } from "./regtest-rpc.mjs";
import { initialize, REGTEST_FAUCET_MNEMONIC, REGTEST_FAUCET_TRANSPARENT } from "../packages/sdk/src/lab.ts";
import { createWasmClient } from "../packages/sdk/src/wasm-client.ts";
import { grpcWebTransport } from "../packages/sdk/src/lwd.ts";

const memoFetch = process.env.Z_STACK_MEMO_FETCH ?? "shared";
assert.ok(["shared", "auto"].includes(memoFetch));
assertLocalRegtestChain(await zebraRpc("getblockchaininfo"), ZEBRA_RPC);
const proxy = createGrpcWebProxy({ upstream: regtestLwdUrl(), allowTransparent: false });
proxy.listen(0, "127.0.0.1"); await once(proxy, "listening");
const url = `http://127.0.0.1:${proxy.address().port}`;
const requests = [];
const fetchOriginal = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const path = new URL(String(input)).pathname;
  if (String(input).startsWith(url)) {
    requests.push(path);
    assert.ok(!/GetAddress|GetTaddress/.test(path), "address query forbidden");
    if (memoFetch === "shared") assert.ok(!/GetTransaction$/.test(path), "shared mode leaked a transaction lookup");
    else assert.notEqual(path, "/zstack/memos", "selective mode downloaded a shared memo range");
    assert.equal(init?.credentials, "omit");
    assert.equal(init?.referrerPolicy, "no-referrer");
  }
  return fetchOriginal(input, init);
};
try {
  await initialize({ wasmModule: readFileSync(new URL("../packages/sdk/src/generated/z_wasm_bg.wasm", import.meta.url)),
    regtestNu63Height: Number(process.env.Z_STACK_REGTEST_NU6_3 || 150),
    regtestNu7Height: process.env.Z_STACK_REGTEST_NU7 === undefined ? undefined : Number(process.env.Z_STACK_REGTEST_NU7), prewarmProvingKey: false, prewarmProveWorker: false });
  const transport = grpcWebTransport(url, { sharedMemos: true });
  const wallet = createWasmClient({ network: "regtest", transport, memoFetch, transparentScan: "compact", autoShield: false });
  let state = await wallet.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1);
  assert.equal(state.transparentAddress, REGTEST_FAUCET_TRANSPARENT);
  state = await wallet.sync();
  assert.equal(state.transparentScanStatus, "complete");
  assert.equal(state.transparentScanHeight, state.scannedHeight);
  assert.ok(state.balance.transparentAvailable > 100000, "mature coinbase discovered locally");
  while (state.memoFetchStatus === "scanning") state = await wallet.sync();
  assert.equal(state.memoFetchStatus, "complete");
  console.log(`PASS local deposit scan and ${memoFetch} memo coverage`);
  await wallet.shield(100000);
  await generate(2);
  const height = (await zebraRpc("getblockchaininfo")).blocks;
  for (let i = 0; i < 100 && await transport.tip() < height; i++) await new Promise(r => setTimeout(r, 200));
  state = await wallet.sync();
  assert.equal(state.transparentScanStatus, "complete");
  console.log("PASS shield proof and broadcast from locally scanned transparent outputs");
  const recipient = state.unifiedAddress;
  const sent = await wallet.send(recipient, "0.001", "Shared retrieval acceptance");
  assert.ok(sent.txid);
  await generate(2);
  const end = (await zebraRpc("getblockchaininfo")).blocks;
  for (let i = 0; i < 100 && await transport.tip() < end; i++) await new Promise(r => setTimeout(r, 200));
  // Discard local send metadata: the memo must be recovered through the selected retrieval mode.
  await wallet.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1);
  state = await wallet.sync();
  while (state.memoFetchStatus === "scanning") state = await wallet.sync();
  const history = await wallet.history(100);
  assert.ok(history.some(row => row.memos?.includes("Shared retrieval acceptance")), "memo decrypted after fresh restore");
  assert.equal(state.memoFetchStatus, "complete");
  const txReads = requests.filter(path => /GetTransaction$/.test(path)).length;
  await wallet.sync();
  assert.equal(requests.filter(path => /GetTransaction$/.test(path)).length, txReads, "completed memos were fetched again");
  console.log(`PASS real mined memo via ${memoFetch} retrieval and no repeat transaction queries`);
  console.log(JSON.stringify({ scannedHeight: state.scannedHeight, memoHeight: state.memoScanHeight,
    sharedRangeRequests: requests.filter(path => path === "/zstack/memos").length,
    memoFetch, selectiveClientRequests: txReads }));
} finally {
  globalThis.fetch = fetchOriginal;
  proxy.closeAllConnections(); await new Promise(resolve => proxy.close(resolve));
}
