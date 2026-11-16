import test from 'node:test';
import assert from 'node:assert/strict';
import { fixture, owner as signer, payer } from '../../solana/test/fixtures';
import { prepareJupiterSwap, validateSignedSolanaSwap, coSignSolanaSwap, solanaTransactionSignature, SOLANA_CYPH, SOLANA_USDC } from '../../solana/src/index';
import { createTwoLegController, createMemoryTwoLegRouteStore, IntermediateSettlementSchema, type TwoLegRouteAdapters, type RouteOwner } from '../src/index';
const owner: RouteOwner = { profileId: 'disposable-profile', walletFingerprint: 'fixture-only', solanaAddress: signer.publicKey.toBase58() };
const near = { providerReference: 'near-quote-fixture', sourceAssetId: 'zec-mainnet', sourceAmount: '10000', depositAddress: 'disposable-transparent-fixture', depositMemo: null, refundAddress: 'disposable-refund-fixture', destinationMint: SOLANA_USDC.mint, destinationOwner: owner.solanaAddress, minimumDestinationAmount: '90', expiresAt: Date.now() + 600000 };
const proof = { providerReference: near.providerReference, signature: 'finalized-first-leg-disposable-signature', owner: owner.solanaAddress, mint: SOLANA_USDC.mint, amount: '100', slot: 123, finality: 'finalized' as const };
function adapters(overrides: Partial<TwoLegRouteAdapters> = {}): TwoLegRouteAdapters {
  return { verifyIntermediate: async (_route, evidence) => IntermediateSettlementSchema.parse(evidence), submitJupiter: async () => { throw new Error('not configured'); }, refreshNear: async () => ({ status: 'success', checkedAt: Date.now() }), reconcileJupiter: async () => ({ kind: 'unknown' }), ...overrides };
}
async function ready(controller: ReturnType<typeof createTwoLegController>, id = 'route-1') {
  await controller.create({ id, near }); await controller.recordNearObservation(id, { status: 'success', checkedAt: Date.now() }); return controller.acceptIntermediate(id, proof);
}
test('concurrent funding claims persist one unknown reservation, and resume never resends', async () => {
  const store = createMemoryTwoLegRouteStore(), controller = createTwoLegController({ owner, store, adapters: adapters() });
  await controller.create({ id: 'route-1', near });
  const claims = await Promise.allSettled([controller.claimFunding('route-1'), controller.claimFunding('route-1')]);
  assert.equal(claims.filter(result => result.status === 'fulfilled').length, 1);
  assert.equal((await controller.get('route-1')).state.kind, 'source-funding-unknown');
  assert.equal((await controller.resume('route-1')).state.kind, 'awaiting-settlement-proof');
  await assert.rejects(controller.acceptIntermediate('route-1', { ...proof, owner: payer.publicKey.toBase58() }), /match/);
  assert.equal((await controller.acceptIntermediate('route-1', proof)).state.kind, 'intermediate-ready');
});
test('timeout persists exact bytes, resume reads only, explicit retry reuses identical signature authorization', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap(f), tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([signer]);
  const signed = Buffer.from(tx.serialize()).toString('base64'), final = coSignSolanaSwap(prepared, signed, payer), signature = solanaTransactionSignature(final);
  const sent: string[] = [];
  const controller = createTwoLegController({ owner, store: createMemoryTwoLegRouteStore(), adapters: adapters({ submitJupiter: async (_route, _prepared, value) => { sent.push(value); if (sent.length === 1) throw new Error('response timeout'); return { signature }; } }) });
  await ready(controller); await controller.saveJupiterReview('route-1', prepared);
  await assert.rejects(controller.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed }), /timeout/);
  assert.equal((await controller.get('route-1')).state.kind, 'jupiter-submission-unknown');
  await controller.resume('route-1'); assert.equal(sent.length, 1);
  await assert.rejects(controller.saveJupiterReview('route-1', prepared), /not ready/);
  assert.equal((await controller.retryJupiter('route-1')).state.kind, 'jupiter-processing');
  assert.deepEqual(sent, [signed, signed]);
});
test('failed second leg preserves intermediate funds and only a finalized matching output completes', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap(f), tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([signer]);
  const signed = Buffer.from(tx.serialize()).toString('base64'), transaction = coSignSolanaSwap(prepared, signed, payer), signature = solanaTransactionSignature(transaction);
  let failed = true;
  const controller = createTwoLegController({ owner, store: createMemoryTwoLegRouteStore(), adapters: adapters({ submitJupiter: async () => ({ signature }), reconcileJupiter: async () => failed ? { kind: 'failed', reason: 'finalized on-chain failure', signature, transaction, finality: 'finalized' } : { kind: 'completed', signature, transaction, owner: owner.solanaAddress, mint: SOLANA_CYPH.mint, outputAmount: '200', finality: 'finalized' } }) });
  await ready(controller); await controller.saveJupiterReview('route-1', prepared); await controller.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed });
  const recovered = await controller.resume('route-1'); assert.equal(recovered.state.kind, 'intermediate-recoverable');
  assert.equal('settlement' in recovered.state && recovered.state.settlement.amount, '100');
  failed = false; await controller.saveJupiterReview('route-1', prepared); await controller.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed });
  assert.equal((await controller.resume('route-1')).state.kind, 'completed');
});
test('storage failure precedes transmission, provider success alone never spends, original owner namespace survives', async () => {
  let submitted = 0;
  const store = createMemoryTwoLegRouteStore();
  const controller = createTwoLegController({ owner, store, adapters: adapters({ submitJupiter: async () => { submitted++; throw new Error(); } }) });
  await controller.create({ id: 'route-1', near });
  await controller.resume('route-1'); assert.equal(submitted, 0);
  const other = createTwoLegController({ owner: { ...owner, profileId: 'different-profile' }, store, adapters: adapters() });
  assert.deepEqual(await other.list(), []);
  await assert.rejects(other.get('route-1'), /not found/);
  const broken = createTwoLegController({ owner, store: { transaction: async () => { throw new Error('quota failure'); } }, adapters: adapters() });
  await assert.rejects(broken.create({ id: 'route-2', near }), /quota/); assert.equal(submitted, 0);
});

test('only an exact provider cancellation can release a never-acknowledged submission', async () => {
  const f = fixture(), prepared = { ...await prepareJupiterSwap(f), providerQuoteId: 'provider_quote_disposable' };
  const tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([signer]);
  const signed = Buffer.from(tx.serialize()).toString('base64');
  let wrong = true;
  const controller = createTwoLegController({ owner, store: createMemoryTwoLegRouteStore(), adapters: adapters({
    submitJupiter: async () => { throw new Error('request did not receive an acknowledgement'); },
    reconcileJupiter: async () => ({ kind: 'not-sent', providerQuoteId: wrong ? 'different_quote' : prepared.providerQuoteId, reviewedMessage: prepared.message, reason: 'Backend atomically canceled unsigned quote' }),
  }) });
  await ready(controller); await controller.saveJupiterReview('route-1', prepared);
  await assert.rejects(controller.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed }));
  await assert.rejects(controller.resume('route-1'), /Cancellation does not match/);
  assert.equal((await controller.get('route-1')).state.kind, 'jupiter-submission-unknown');
  wrong = false;
  assert.equal((await controller.resume('route-1')).state.kind, 'intermediate-recoverable');
});

test('days-later restored route releases only past its saved RPC conservative expiry bound', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap(f), tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([signer]);
  const signed = Buffer.from(tx.serialize()).toString('base64'), transaction = coSignSolanaSwap(prepared, signed, payer), signature = solanaTransactionSignature(transaction);
  const store = createMemoryTwoLegRouteStore();
  const original = createTwoLegController({ owner, store, adapters: adapters({ submitJupiter: async () => { throw new Error('acknowledgement lost'); } }) });
  await ready(original); await original.saveJupiterReview('route-1', prepared);
  await assert.rejects(original.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed }));
  let finalizedBlockHeight = 110;
  const restored = createTwoLegController({ owner, store, now: () => Date.now() + 3 * 86400000, adapters: adapters({ reconcileJupiter: async () => ({
    kind: 'not-included', signature, transaction, proof: { kind: 'finalized-block-signatures', signature, blockhash: tx.message.recentBlockhash, fromSlot: 1, throughSlot: 3, checkedBlocks: 3, finalizedBlockHeight, lastValidBlockHeight: 100, conservativeExpiryBlockHeight: 120, throughBlockhash: tx.message.recentBlockhash, blockhashValid: false },
  }) }) });
  await assert.rejects(restored.resume('route-1'), /Non-inclusion coverage/);
  assert.equal((await restored.get('route-1')).state.kind, 'jupiter-submission-unknown');
  finalizedBlockHeight = 121;
  assert.equal((await restored.resume('route-1')).state.kind, 'intermediate-recoverable');
});

test('polling and reopening an unfunded pending deposit preserves its one funding claim', async () => {
  const store = createMemoryTwoLegRouteStore(), pending = adapters({ refreshNear: async () => ({ status: 'pending', checkedAt: Date.now() }) });
  const controller = createTwoLegController({ owner, store, adapters: pending });
  await controller.create({ id: 'route-1', near });
  assert.equal((await controller.resume('route-1')).state.kind, 'awaiting-source-funding');
  const reopened = createTwoLegController({ owner, store, adapters: pending });
  assert.equal((await reopened.resume('route-1')).state.kind, 'awaiting-source-funding');
  const claim = await reopened.claimFunding('route-1');
  assert.ok(claim.claimId);
  assert.equal((await reopened.resume('route-1')).state.kind, 'source-funding-unknown');
  await assert.rejects(reopened.claimFunding('route-1'), /already claimed/);
  await reopened.create({ id: 'route-2', near: { ...near, providerReference: 'second-deposit', depositAddress: 'second-disposable-address' } });
  await reopened.recordNearObservation('route-2', { status: 'processing', checkedAt: Date.now() });
  assert.equal((await reopened.get('route-2')).state.kind, 'near-processing');
  await assert.rejects(reopened.claimFunding('route-2'), /already claimed/);
});

test('twentieth attempt finalizes durably and a new review is rejected without dropping history', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap(f), tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([signer]);
  const signed = Buffer.from(tx.serialize()).toString('base64'), transaction = coSignSolanaSwap(prepared, signed, payer), signature = solanaTransactionSignature(transaction);
  const store = createMemoryTwoLegRouteStore(); let submitted = 0;
  const controller = createTwoLegController({ owner, store, adapters: adapters({ submitJupiter: async () => { submitted++; return { signature }; }, reconcileJupiter: async () => ({ kind: 'failed', signature, transaction, finality: 'finalized', reason: 'Finalized fixture failure' }) }) });
  await ready(controller);
  const prior = Array.from({ length: 19 }, (_, index) => ({ outcome: 'failed' as const, prepared, signedTransaction: transaction, signature, reason: `Earlier failure ${index}`, observedAt: Date.now() + index }));
  await store.transaction(owner, routes => ({ routes: routes.map(route => ({ ...route, jupiterAttempts: prior })), result: undefined }));
  await controller.saveJupiterReview('route-1', prepared);
  await controller.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed });
  const settled = await controller.resume('route-1');
  assert.equal(settled.jupiterAttempts.length, 20);
  assert.deepEqual(settled.jupiterAttempts.slice(0, 19), prior);
  assert.equal(settled.state.kind, 'intermediate-recoverable');
  await assert.rejects(controller.saveJupiterReview('route-1', prepared), /20-attempt limit.*funds remain/);
  assert.equal(submitted, 1);
  assert.deepEqual((await controller.get('route-1')).jupiterAttempts, settled.jupiterAttempts);
});

test('fresh execute and retry capacity checks stop transmission while legacy finalized overflow remains recordable', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap(f), tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false }); tx.sign([signer]);
  const signed = Buffer.from(tx.serialize()).toString('base64'), transaction = coSignSolanaSwap(prepared, signed, payer), signature = solanaTransactionSignature(transaction);
  const store = createMemoryTwoLegRouteStore(); let submitted = 0;
  const controller = createTwoLegController({ owner, store, adapters: adapters({ submitJupiter: async () => { submitted++; return { signature }; }, reconcileJupiter: async () => ({ kind: 'failed', signature, transaction, finality: 'finalized', reason: 'Legacy in-flight failure' }) }) });
  await ready(controller); await controller.saveJupiterReview('route-1', prepared);
  const prior = Array.from({ length: 20 }, (_, index) => ({ outcome: 'failed' as const, prepared, signedTransaction: transaction, signature, reason: `Earlier failure ${index}`, observedAt: Date.now() + index }));
  await store.transaction(owner, routes => ({ routes: routes.map(route => ({ ...route, jupiterAttempts: prior })), result: undefined }));
  await assert.rejects(controller.executeJupiter('route-1', { accepted: true, reviewedMessage: prepared.message, signedTransaction: signed }), /20-attempt limit.*funds remain/);
  const unchanged = await controller.get('route-1');
  assert.equal(unchanged.state.kind, 'awaiting-jupiter-review');
  assert.equal(submitted, 0);
  assert.ok('settlement' in unchanged.state);
  const settlement = unchanged.state.settlement;
  // Reproduce an old-version reservation that had already reached the network.
  await store.transaction(owner, routes => ({ routes: routes.map(route => ({ ...route, state: { kind: 'jupiter-submission-unknown' as const, settlement, prepared, signedTransaction: signed, signature: null } })), result: undefined }));
  await assert.rejects(controller.retryJupiter('route-1'), /20-attempt limit.*funds remain/);
  assert.equal(submitted, 0);
  const finalized = await controller.resume('route-1');
  assert.equal(finalized.state.kind, 'intermediate-recoverable');
  assert.equal(finalized.jupiterAttempts.length, 21);
  assert.deepEqual(finalized.jupiterAttempts.slice(0, 20), prior);
});
