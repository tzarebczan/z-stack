import assert from 'node:assert/strict';
import test from 'node:test';
import { Buffer } from 'buffer';
import { PublicKey } from '@solana/web3.js';
import { fixture, owner, payer } from './fixtures';
import { prepareJupiterSwap, validateSignedSolanaSwap, coSignSolanaSwap, solanaTransactionSignature, proveSolanaNotIncluded, type SolanaRpc } from '../src/index';
const blockhash = (slot: number) => { const bytes = new Uint8Array(32); new DataView(bytes.buffer).setUint32(0, slot); return new PublicKey(bytes).toBase58(); };
const historicalSlots = Array.from({ length: 152 }, (_, index) => 1000 + index);
const header = (slot: number) => ({ blockhash: blockhash(slot), previousBlockhash: blockhash(slot - 1), parentSlot: slot - 1, blockTime: null, blockHeight: 60 + slot - 1000 });
async function setup(jupiterExpiry = 200) {
  const f = fixture(); f.build.blockhashWithMetadata.lastValidBlockHeight = jupiterExpiry;
  const simulate = f.rpc.simulateTransaction.bind(f.rpc);
  const preparationRpc = { ...f.rpc,
    getBlockHeight: async () => 60,
    simulateTransaction: async (...args: Parameters<SolanaRpc['simulateTransaction']>) => ({ ...await simulate(...args), context: { slot: 1000 } }),
    isBlockhashValid: async () => ({ context: { slot: 1000 }, value: true }),
    getLatestBlockhashAndContext: async () => ({ context: { slot: 1000 }, value: { blockhash: blockhash(1000), lastValidBlockHeight: 210 } }),
  } as SolanaRpc;
  const prepared = await prepareJupiterSwap({ ...f, rpc: preparationRpc });
  const tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([owner]);
  const signature = solanaTransactionSignature(coSignSolanaSwap(prepared, Buffer.from(tx.serialize()).toString('base64'), payer));
  const requestedRanges: number[][] = [];
  const rpc = { ...preparationRpc,
    getSignatureStatuses: async () => ({ context: { slot: 1_000_000 }, value: [null] }),
    getTransaction: async () => null,
    isBlockhashValid: async () => ({ context: { slot: 1_000_000 }, value: false }),
    getFirstAvailableBlock: async () => 0,
    getBlocks: async (from: number, through: number) => { requestedRanges.push([from, through]); return historicalSlots; },
    getBlockSignatures: async (slot: number) => ({ ...header(slot), signatures: [] }),
    getBlock: async (slot: number, config: unknown) => { assert.deepEqual(config, { commitment: 'finalized', transactionDetails: 'none', rewards: false, maxSupportedTransactionVersion: 0 }); return header(slot); },
  } as SolanaRpc;
  return { rpc, prepared, signature, requestedRanges };
}
test('days-later reload scans only complete historical validity window, bounded by selected RPC expiry', async () => {
  const value = await setup();
  const proof = await proveSolanaNotIncluded(value);
  assert.deepEqual(value.requestedRanges, [[1000, 1299]]);
  assert.deepEqual(proof, { kind: 'finalized-block-signatures', signature: value.signature, blockhash: validateSignedSolanaSwap(value.prepared, value.prepared.transaction, { requireOwnerSignature: false }).message.recentBlockhash, fromSlot: 1000, throughSlot: 1151, checkedBlocks: 152, finalizedBlockHeight: 211, lastValidBlockHeight: 200, conservativeExpiryBlockHeight: 210, throughBlockhash: blockhash(1151), blockhashValid: false });
  assert.equal(value.prepared.review.conservativeExpiryBlockHeight, 210);
});
test('expiry-height boundary, missing/changed finalized headers and incomplete signature chains remain unknown', async () => {
  const value = await setup();
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlocks: async () => historicalSlots.filter(slot => slot !== 1050) } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getFirstAvailableBlock: async () => 1001 } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlock: async () => ({ ...header(1151), blockHeight: 210 }) } as SolanaRpc }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlock: async () => null } }), null);
  let reads = 0;
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlock: async () => ({ ...header(1151), blockhash: blockhash(reads++ ? 1152 : 1151) }) } as SolanaRpc }), null);
});
test('malformed Jupiter expiry cannot prematurely release and cannot replace the RPC bound', async () => {
  const short = await setup(61);
  assert.equal(short.prepared.review.lastValidBlockHeight, 61);
  assert.equal(short.prepared.review.conservativeExpiryBlockHeight, 210);
  assert.equal(await proveSolanaNotIncluded({ ...short, rpc: { ...short.rpc, getBlock: async () => ({ ...header(1151), blockHeight: 100 }) } as SolanaRpc }), null);
  const long = await setup(10000);
  assert.equal((await proveSolanaNotIncluded(long))?.conservativeExpiryBlockHeight, 210);
});
test('a historical null, an invalid hash or a pruned provider is insufficient without all block signatures', async () => {
  const value = await setup();
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlockSignatures: async () => { throw new Error('history pruned'); } } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlockSignatures: async slot => ({ ...header(slot), signatures: slot === 1050 ? [value.signature] : [] }) } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, isBlockhashValid: async () => ({ context: { slot: 1_000_000 }, value: true }) } }), null);
});
