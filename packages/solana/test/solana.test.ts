import test from 'node:test';
import assert from 'node:assert/strict';
import { Buffer } from 'buffer';
import { HDKey } from 'micro-key-producer/slip10.js';
import { Keypair, PublicKey, SystemProgram, VersionedTransaction, TransactionInstruction } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token';
import { deriveSolanaAddress, signSolanaMessage, verifySolanaSignature, withSolanaSigner, prepareJupiterSwap, signPreparedSolanaSwap, coSignSolanaSwap, validateSignedSolanaSwap, PreparedSolanaSwapSchema, JUPITER_PROGRAM, getSolanaTransactionStatus, type SolanaRpc, SOLANA_USDC, SOLANA_CYPH } from '../src/index';
const phrase = 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about';
import { fixture, owner, payer } from './fixtures';

test('SLIP10 vector 1 root and hardened child are exact', () => {
  const root = HDKey.fromMasterSeed(Buffer.from('000102030405060708090a0b0c0d0e0f', 'hex'));
  assert.equal(Buffer.from(root.privateKey).toString('hex'), '2b4be7f19ee27bbf30c667b642d5f4aa69fd169872f8fc3059c08ebae2eb19e7');
  assert.equal(Buffer.from(root.derive("m/0'").privateKey).toString('hex'), '68e0fe46dfb67e368c75379acec591dad19df3cde26e63b93a8e704f1dade7a3');
});
test('same phrase restores standard Solana address and proof; callback key is erased', async () => {
  const address = deriveSolanaAddress(phrase);
  assert.equal(address, 'HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk');
  const message = new TextEncoder().encode('disposable ownership proof');
  assert.equal(verifySolanaSignature(address, message, signSolanaMessage(phrase, message)), true);
  assert.equal(verifySolanaSignature(address, new Uint8Array([0]), signSolanaMessage(phrase, message)), false);
  let retained: Keypair | undefined;
  await withSolanaSigner(phrase, signer => { retained = signer; });
  assert.equal(retained?.secretKey.every(byte => byte === 0), true);
});
test('real compile, owner signature, bounded sponsor signature preserve exact reviewed message', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap(f);
  PreparedSolanaSwapSchema.parse(prepared);
  const signed = await signPreparedSolanaSwap(prepared, { rpc: f.rpc, tokens: f.tokens, guard: { assertCurrent() {} }, withSigner: async run => run(owner) });
  const final = coSignSolanaSwap(prepared, signed, payer);
  assert.equal(validateSignedSolanaSwap(prepared, final, { requireOwnerSignature: true, requirePayerSignature: true }).message.recentBlockhash, VersionedTransaction.deserialize(Buffer.from(prepared.transaction, 'base64')).message.recentBlockhash);
  const changed = VersionedTransaction.deserialize(Buffer.from(signed, 'base64')); changed.message.recentBlockhash = Keypair.generate().publicKey.toBase58();
  assert.throws(() => coSignSolanaSwap(prepared, Buffer.from(changed.serialize()).toString('base64'), payer), /changed/);
  assert.throws(() => coSignSolanaSwap(prepared, prepared.transaction, payer), /signature/);
});
test('authority mutation, unrelated programs, fake amounts and added signers fail before sign', async () => {
  const f = fixture(); const token = f.intent.inputMint; const accountKey = [...f.after.keys()].find(key => f.after.get(key)?.data.length === 165 && f.after.get(key)?.data.subarray(0,32).equals(new PublicKey(token).toBuffer())); assert.ok(accountKey); f.after.set(accountKey, f.account(new PublicKey(token), 900n, payer.publicKey));
  await assert.rejects(prepareJupiterSwap(f), /authority/);
  const g = fixture(); g.build.otherInstructions.push(g.build.swapInstruction as never);
  await assert.rejects(prepareJupiterSwap(g), /added/);
  const h = fixture(); h.build.inAmount = '101'; await assert.rejects(prepareJupiterSwap(h), /changed/);
  const i = fixture(); i.build.swapInstruction.accounts[3]!.isSigner = true; await assert.rejects(prepareJupiterSwap(i), /signer/);
});
test('expired quotes and changed session cannot reach signer', async () => {
  const f = fixture(), prepared = await prepareJupiterSwap({ ...f, now: 1 }); let called = false;
  await assert.rejects(signPreparedSolanaSwap(prepared, { rpc: f.rpc, tokens: f.tokens, guard: { assertCurrent() {} }, withSigner: async run => { called = true; return run(owner); } }), /expired/);
  const fresh = await prepareJupiterSwap(f);
  await assert.rejects(signPreparedSolanaSwap(fresh, { rpc: f.rpc, tokens: f.tokens, guard: { assertCurrent() { throw new Error('changed session'); } }, withSigner: async run => { called = true; return run(owner); } }), /changed session/);
  assert.equal(called, false);
});
test('expiry without historical transaction evidence stays unresolved', async () => {
  const status = await getSolanaTransactionStatus({ getSignatureStatuses: async () => ({ context: { slot: 1 }, value: [null] }), getBlockHeight: async () => 101 }, 'disposable', 100);
  assert.deepEqual(status, { kind: 'expired-unresolved', signature: 'disposable' });
});

test('RPC conservative expiry never extends signing eligibility or replaces the signed blockhash', async () => {
  const expired = fixture(); expired.build.blockhashWithMetadata.lastValidBlockHeight = 10;
  await assert.rejects(prepareJupiterSwap(expired), /expired/);
  const invalid = fixture(); invalid.rpc.isBlockhashValid = async () => ({ context: { slot: 1 }, value: false });
  await assert.rejects(prepareJupiterSwap(invalid), /expired or could not be verified/);
  const value = fixture(), prepared = await prepareJupiterSwap(value);
  const transaction = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false });
  assert.equal(prepared.review.lastValidBlockHeight, 100);
  assert.equal(prepared.review.conservativeExpiryBlockHeight, 120);
  assert.notEqual(transaction.message.recentBlockhash, (await value.rpc.getLatestBlockhashAndContext()).value.blockhash);
  let signed = false;
  await assert.rejects(signPreparedSolanaSwap({ ...prepared, review: { ...prepared.review, conservativeExpiryBlockHeight: 10 } }, { rpc: value.rpc, tokens: value.tokens, guard: { assertCurrent() {} }, withSigner: async run => { signed = true; return run(owner); } }), /expired/);
  assert.equal(signed, false);
});

test('a blockhash that expires during fresh unlock is never signed', async () => {
  const value = fixture(), prepared = await prepareJupiterSwap(value);
  await assert.rejects(signPreparedSolanaSwap(prepared, { rpc: value.rpc, tokens: value.tokens, guard: { assertCurrent() {} }, withSigner: async run => {
    value.rpc.isBlockhashValid = async () => ({ context: { slot: 1 }, value: false });
    return run(owner);
  } }), /expired/);
});
