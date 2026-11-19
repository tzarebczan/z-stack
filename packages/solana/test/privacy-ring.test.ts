import assert from 'node:assert/strict';
import test from 'node:test';
import bs58 from 'bs58';
import { getSolanaRingAssetSupport, SOLANA_PRIVACY_RING_PROGRAM, SOLANA_USDC, verifySolanaRingDeposit } from '../src/index';
import type { SolanaRingRead } from '../src/index';

const genesis = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const expected = {
  signature: '2UYGWAmS5nRYs84vEquoH75SRKLtDeMnq6u4945bM4iuHUCQLEZXX7XdZtYj9N51k61NThjRHbx8d4cq1Bjxdjne',
  depositor: '5QJSupDFJWqEmiXSxVMP8isABpW6Rs192kP8eLFrDqnd',
  recipient: 'DK9wGq1cbMjHKzGaKtB4LZwmWhz7LvqoFgSCAh1imG6c',
  ownerHash: '3iaA1s3x7RdMiJHEw7XHRRZ3HMAUZf8YixAn1EonQrvk',
  token: SOLANA_USDC,
  minimumAmount: '299000000',
};
const tree = '777pvSGe5LaBd9nCczvE2FXEauiwtfGJ5XRpjKyniGor';
const vault = 'H4RD4XSWp8TLmrNYCLmV74a5ixude5s9gGqpCFjXkBLQ';

function receipt(err: unknown = null) {
  const data = Buffer.concat([
    Buffer.from([15, 1, 1, 250, 1, 0]),
    bs58.decode(expected.recipient),
    bs58.decode(expected.ownerHash),
    Buffer.from('00a3e1110000000000', 'hex'),
  ]);
  return {
    version: 1,
    slot: 471244421,
    transaction: {
      signatures: [expected.signature],
      message: {
        accountKeys: [expected.depositor, SOLANA_PRIVACY_RING_PROGRAM, tree, SOLANA_USDC.program,
          SOLANA_USDC.mint, 'B6DPLFC2vm9QCAaAt6XZsQCM7gziabeMaekvcbh1E8KD'],
        instructions: [{ programIdIndex: 1, accounts: [2, 0, 1, 3, 4, 5, 6], data: bs58.encode(data) }],
      },
    },
    meta: { err, loadedAddresses: { writable: [vault], readonly: [] } },
  };
}

function readResult(result: unknown): SolanaRingRead {
  return async request => request.method === 'getGenesisHash' ? genesis : result;
}

test('checks finality on mainnet and resolves version 1 lookup accounts to verify Ring delivery', async () => {
  const requests: Parameters<SolanaRingRead>[0][] = [];
  const result = await verifySolanaRingDeposit(request => {
    requests.push(request);
    return readResult(receipt())(request);
  }, expected);
  assert.deepEqual(result, { kind: 'finalized', signature: expected.signature, slot: 471244421, amount: '300000000' });
  assert.deepEqual(requests, [
    { method: 'getGenesisHash', params: [] },
    { method: 'getTransaction', params: [expected.signature, { commitment: 'finalized', encoding: 'json', maxSupportedTransactionVersion: 1 }] },
  ]);
});

test('an unknown receipt stays pending and a different chain fails closed', async () => {
  assert.deepEqual(await verifySolanaRingDeposit(readResult(null), expected), { kind: 'pending' });
  await assert.rejects(verifySolanaRingDeposit(async () => 'other chain', expected), /require Solana mainnet/);
});

test('rejects a public payout and a finalized deposit with mismatched signing, assets or recipient', async () => {
  const publicPayout = receipt();
  publicPayout.transaction.message.instructions[0]!.programIdIndex = 3;
  await assert.rejects(verifySolanaRingDeposit(readResult(publicPayout), expected), /did not match/);
  const wrongVault = receipt();
  wrongVault.meta.loadedAddresses.writable[0] = tree;
  await assert.rejects(verifySolanaRingDeposit(readResult(wrongVault), expected), /did not match/);
  for (const change of [
    { ownerHash: tree },
    { recipient: tree },
    { depositor: tree },
    { token: { ...SOLANA_USDC, mint: tree } },
    { minimumAmount: '300000001' },
  ]) {
    await assert.rejects(verifySolanaRingDeposit(readResult(receipt()), { ...expected, ...change }), /did not match/);
  }
});

test('rejects failed, wrong-signature and ambiguous receipts', async () => {
  const failed = receipt({ InstructionError: [0, 'InvalidInstructionData'] });
  await assert.rejects(verifySolanaRingDeposit(readResult(failed), expected), /Successful finalized/);

  const wrongSignature = receipt();
  wrongSignature.transaction.signatures[0] = bs58.encode(new Uint8Array(64).fill(3));
  await assert.rejects(verifySolanaRingDeposit(readResult(wrongSignature), expected), /Successful finalized/);

  const duplicate = receipt();
  duplicate.transaction.message.instructions.push(...duplicate.transaction.message.instructions);
  await assert.rejects(verifySolanaRingDeposit(readResult(duplicate), expected), /ambiguous/);
});

test('checks finalized mint registration and the canonical token vault for Ring asset readiness', async () => {
  const registryAccount = { owner: SOLANA_PRIVACY_RING_PROGRAM, executable: false };
  const vaultAccount = { owner: SOLANA_USDC.program, executable: false };
  const result = await getSolanaRingAssetSupport(request => {
    if (request.method === 'getGenesisHash') return Promise.resolve(genesis);
    assert.deepEqual(request, {
      method: 'getMultipleAccounts',
      params: [['J85i28ZES8pHqDa21YsmLSbUh2FR56c9WW5GCfQXUeHB', vault], {
        encoding: 'base64', commitment: 'finalized', dataSlice: { offset: 0, length: 0 },
      }],
    });
    return Promise.resolve({ value: [registryAccount, vaultAccount] });
  }, SOLANA_USDC);
  assert.deepEqual(result, { kind: 'registered' });
  for (const accounts of [[null, vaultAccount], [registryAccount, null], [registryAccount, registryAccount]]) {
    assert.deepEqual(await getSolanaRingAssetSupport(readResult({ value: accounts }), SOLANA_USDC), { kind: 'unavailable' });
  }
});
