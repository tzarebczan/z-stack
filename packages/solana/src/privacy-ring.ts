import bs58 from 'bs58';
import { PublicKey } from '@solana/web3.js';
import { z } from 'zod';
import { AtomicAmountSchema, SolanaAddressSchema, SolanaTokenSchema } from './tokens';

const MAINNET_GENESIS = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
export const SOLANA_PRIVACY_RING_PROGRAM = 'ring1GaLCuJnkUMVkX2JeZLTTX17wKsV4fZmPVzXANW';
const program = new PublicKey(SOLANA_PRIVACY_RING_PROGRAM);
const encoder = new TextEncoder();
const [defaultTree] = PublicKey.findProgramAddressSync([encoder.encode('tree'), Uint8Array.of(0, 0)], program);
const signatureSchema = z.string().min(64).max(88)
  .refine(value => bs58.decodeUnsafe(value)?.length === 64, 'Invalid Solana signature');
const addressList = z.array(SolanaAddressSchema).max(256);
const accountIndex = z.number().int().min(0).max(255);
const transactionSchema = z.object({
  version: z.union([z.literal('legacy'), z.literal(0), z.literal(1)]),
  slot: z.number().int().safe().positive(),
  transaction: z.object({
    signatures: z.array(signatureSchema).min(1),
    message: z.object({
      accountKeys: addressList,
      instructions: z.array(z.object({
        programIdIndex: accountIndex,
        accounts: z.array(accountIndex).max(256),
        data: z.string().max(2048),
      })),
    }),
  }),
  meta: z.object({
    err: z.unknown(),
    loadedAddresses: z.object({ writable: addressList, readonly: addressList }).optional(),
  }).nullable(),
});

export type SolanaRingRead = (
  request:
    | { method: 'getGenesisHash'; params: [] }
    | { method: 'getTransaction'; params: [string, {
      commitment: 'finalized';
      encoding: 'json';
      maxSupportedTransactionVersion: 1;
    }] },
) => Promise<unknown>;

export interface SolanaRingDepositExpectation {
  /** Destination signature independently bound to the reviewed provider execution. */
  signature: string;
  /** The bridge's expected Solana deposit signer. */
  depositor: string;
  /** Public Solana address of the registered Ring recipient. */
  recipient: string;
  /** Base58-encoded owner hash resolved from the recipient's trusted Ring registry record. */
  ownerHash: string;
  token: z.input<typeof SolanaTokenSchema>;
  minimumAmount: string;
}

/**
 * Verify a finalized single-token proofless deposit into the default Ring.
 * The transport must return JSON-RPC result values from a trusted Solana RPC.
 * This verifies the public deposit. The recipient still syncs its private balance.
 */
export async function verifySolanaRingDeposit(read: SolanaRingRead, expected: SolanaRingDepositExpectation) {
  const signature = signatureSchema.parse(expected.signature);
  const depositor = SolanaAddressSchema.parse(expected.depositor);
  const recipient = SolanaAddressSchema.parse(expected.recipient);
  const ownerHash = SolanaAddressSchema.parse(expected.ownerHash);
  const token = SolanaTokenSchema.parse(expected.token);
  const minimum = BigInt(AtomicAmountSchema.parse(expected.minimumAmount));
  if (minimum === 0n) throw new Error('Ring deposit minimum must be positive.');

  const genesis = await read({ method: 'getGenesisHash', params: [] });
  if (genesis !== MAINNET_GENESIS) throw new Error('Ring deposit requires Solana mainnet.');

  const raw = await read({
    method: 'getTransaction',
    params: [signature, { commitment: 'finalized', encoding: 'json', maxSupportedTransactionVersion: 1 }],
  });
  if (raw === null) return { kind: 'pending' as const };

  const receipt = transactionSchema.parse(raw);
  if (receipt.meta?.err !== null || receipt.transaction.signatures[0] !== signature) {
    throw new Error('Successful finalized Ring transaction is unavailable.');
  }
  const { accountKeys, instructions } = receipt.transaction.message;
  const loaded = receipt.meta.loadedAddresses;
  const keys = [...accountKeys, ...(loaded?.writable ?? []), ...(loaded?.readonly ?? [])];
  const [vault, bump] = PublicKey.findProgramAddressSync(
    [encoder.encode('spl_asset_vault'), new PublicKey(token.mint).toBytes()],
    program,
  );

  let amount: bigint | undefined;
  for (const { programIdIndex, accounts, data } of instructions) {
    if (keys[programIdIndex] !== SOLANA_PRIVACY_RING_PROGRAM || accounts.length !== 7) continue;
    const [tree, signer, pool, tokenProgram, mint, source, tokenVault] = accounts.map(index => keys[index]);
    if (tree !== defaultTree.toBase58() || signer !== depositor || pool !== SOLANA_PRIVACY_RING_PROGRAM
      || tokenProgram !== token.program || mint !== token.mint || source === undefined
      || source === vault.toBase58() || tokenVault !== vault.toBase58()) continue;

    // The proofless single-SPL format has one asset, one output and no memo.
    // Protocol layout: @heliuslabs/spr 0.9.0-alpha, encodeDepositInstructionData.
    const bytes = bs58.decodeUnsafe(data);
    if (!bytes || bytes.length !== 79 || bytes[0] !== 15 || bytes[1] !== 1
      || bytes[2] !== 1 || bytes[3] !== bump || bytes[4] !== 1 || bytes[5] !== 0
      || bytes[78] !== 0 || bs58.encode(bytes.subarray(6, 38)) !== recipient
      || bs58.encode(bytes.subarray(38, 70)) !== ownerHash) continue;

    if (amount !== undefined) throw new Error('Ring deposit receipt is ambiguous.');
    amount = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getBigUint64(70, true);
  }
  if (amount === undefined || amount < minimum) throw new Error('Finalized Ring deposit did not match the reviewed delivery.');

  return { kind: 'finalized' as const, signature, slot: receipt.slot, amount: amount.toString() };
}
