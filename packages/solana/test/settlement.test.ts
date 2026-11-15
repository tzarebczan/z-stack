import assert from 'node:assert/strict';
import test from 'node:test';
import { Buffer } from 'buffer';
import { fixture, owner, payer } from './fixtures';
import { prepareJupiterSwap, validateSignedSolanaSwap, coSignSolanaSwap, solanaTransactionSignature, proveSolanaNotIncluded, type SolanaRpc } from '../src/index';
async function setup() {
  const f = fixture(), prepared = await prepareJupiterSwap(f), tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([owner]);
  const signature = solanaTransactionSignature(coSignSolanaSwap(prepared, Buffer.from(tx.serialize()).toString('base64'), payer));
  const rpc = { ...f.rpc,
    getSignatureStatuses: async () => ({ context: { slot: 3 }, value: [null] }),
    getTransaction: async () => null,
    isBlockhashValid: async () => ({ context: { slot: 3 }, value: false }),
    getFirstAvailableBlock: async () => 0,
    getBlockHeight: async () => 101,
    getBlocks: async () => [1,2,3],
    getBlockSignatures: async (slot: number) => ({ blockhash: `fixture-block-${slot}`, previousBlockhash: `fixture-block-${slot - 1}`, parentSlot: slot - 1, blockTime: null, signatures: [] }),
  } satisfies SolanaRpc;
  return { rpc, prepared, signature };
}
test('noninclusion requires every finalized produced block, exact parent chain and invalid blockhash', async () => {
  const value = await setup();
  const proof = await proveSolanaNotIncluded(value);
  assert.deepEqual(proof, { kind: 'finalized-block-signatures', signature: value.signature, blockhash: validateSignedSolanaSwap(value.prepared, value.prepared.transaction, { requireOwnerSignature: false }).message.recentBlockhash, fromSlot: 1, throughSlot: 3, checkedBlocks: 3, finalizedBlockHeight: 101, lastValidBlockHeight: 100, blockhashValid: false });
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlocks: async () => [1,3] } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getFirstAvailableBlock: async () => 2 } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, isBlockhashValid: async () => ({ context: { slot: 3 }, value: true }) } }), null);
});
test('a historical null, expiry or incomplete provider cannot release an uncertain payment', async () => {
  const value = await setup();
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlockSignatures: async () => { throw new Error('history pruned'); } } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, getBlockSignatures: async slot => ({ blockhash: `fixture-block-${slot}`, previousBlockhash: `fixture-block-${slot - 1}`, parentSlot: slot - 1, blockTime: null, signatures: slot === 2 ? [value.signature] : [] }) } }), null);
  assert.equal(await proveSolanaNotIncluded({ ...value, rpc: { ...value.rpc, isBlockhashValid: async () => ({ context: { slot: 301 }, value: false }) } }), null);
});
