import { Buffer } from 'buffer';
import bs58 from 'bs58';
import { z } from 'zod';
import {
  ComputeBudgetProgram, PublicKey, SystemProgram, TransactionInstruction,
  TransactionMessage, VersionedTransaction, type Keypair, type AccountInfo,
} from '@solana/web3.js';
import {
  ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID,
  getAssociatedTokenAddressSync, unpackAccount, unpackMint, getTransferHook,
  getScaledUiAmountConfig, getPausableConfig,
} from '@solana/spl-token';
import { verifySolanaSignature } from './account';
import { AtomicAmountSchema, SolanaAddressSchema, SOLANA_TOKENS, SOL_MINT, type SolanaToken } from './tokens';
import { type SolanaRpc, type SolanaGuard } from './wallet';

export const JUPITER_PROGRAM = 'JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4';
const integer = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const instructionSchema = z.object({ programId: SolanaAddressSchema, accounts: z.array(z.object({ pubkey: SolanaAddressSchema, isSigner: z.boolean(), isWritable: z.boolean() }).strict()).max(128), data: z.string().max(4096) }).strict();
export const JupiterBuildSchema = z.object({
  inputMint: SolanaAddressSchema, outputMint: SolanaAddressSchema, inAmount: AtomicAmountSchema,
  outAmount: AtomicAmountSchema, otherAmountThreshold: AtomicAmountSchema,
  swapMode: z.literal('ExactIn'), slippageBps: integer.max(10000),
  computeBudgetInstructions: z.array(instructionSchema).max(4), setupInstructions: z.array(instructionSchema).max(16),
  swapInstruction: instructionSchema, cleanupInstruction: instructionSchema.nullable(),
  otherInstructions: z.array(instructionSchema).max(4), tipInstruction: instructionSchema.nullable().optional(),
  addressesByLookupTableAddress: z.record(SolanaAddressSchema, z.array(SolanaAddressSchema).max(256)).nullable(),
  blockhashWithMetadata: z.object({ blockhash: z.array(z.number().int().min(0).max(255)).length(32), lastValidBlockHeight: integer }),
});
const jupiterIntentObject = z.object({
  owner: SolanaAddressSchema, payer: SolanaAddressSchema,
  inputMint: SolanaAddressSchema, outputMint: SolanaAddressSchema, amount: AtomicAmountSchema,
  slippageBps: integer.max(1000), feeBps: integer.max(100),
  feeAccount: SolanaAddressSchema.nullable(), feeMint: SolanaAddressSchema.nullable(),
  maxSponsorLamports: AtomicAmountSchema, maxPriorityFeeLamports: AtomicAmountSchema,
}).strict();
function validIntent(value: z.infer<typeof jupiterIntentObject>): boolean {
  return BigInt(value.amount) > 0n && value.inputMint !== value.outputMint &&
    (value.feeBps === 0 ? value.feeAccount === null && value.feeMint === null : value.feeAccount !== null && (value.feeMint === value.inputMint || value.feeMint === value.outputMint));
}
export const JupiterIntentSchema = jupiterIntentObject.refine(validIntent, 'Invalid Jupiter trade or fee terms');
export type JupiterIntent = z.infer<typeof JupiterIntentSchema>;
export const PreparedSolanaSwapSchema = z.object({
  version: z.literal(1), providerQuoteId: z.string().regex(/^[A-Za-z0-9_-]{1,96}$/).optional(), build: JupiterBuildSchema, transaction: z.string().max(1800), message: z.string().max(1800),
  review: jupiterIntentObject.extend({
    minimumOutputAmount: AtomicAmountSchema, expectedOutputAmount: AtomicAmountSchema,
    lastValidBlockHeight: integer, conservativeExpiryBlockHeight: integer, preparedAtSlot: integer, expiresAt: integer,
  }).refine(validIntent, 'Invalid Jupiter trade or fee terms'),
}).strict();
export type PreparedSolanaSwap = z.infer<typeof PreparedSolanaSwapSchema>;

function deepFreeze<T>(value: T): T {
  if (value && typeof value === 'object') { for (const child of Object.values(value)) deepFreeze(child); Object.freeze(value); }
  return value;
}
function instruction(value: z.infer<typeof instructionSchema>) {
  const data = Buffer.from(value.data, 'base64');
  if (data.toString('base64') !== value.data) throw new Error('Invalid instruction encoding.');
  return new TransactionInstruction({ programId: new PublicKey(value.programId), keys: value.accounts.map(value => ({ ...value, pubkey: new PublicKey(value.pubkey) })), data });
}
function tokenFor(mint: string, tokens: readonly SolanaToken[]): SolanaToken {
  if (mint === SOL_MINT) return { symbol: 'SOL', mint: SOL_MINT, program: TOKEN_PROGRAM_ID.toBase58(), decimals: 9 };
  const token = tokens.find(token => token.mint === mint);
  if (!token) throw new Error('Unsupported Solana asset.');
  return token;
}
function ata(owner: string, mint: string, program: string) {
  return getAssociatedTokenAddressSync(new PublicKey(mint), new PublicKey(owner), false, new PublicKey(program)).toBase58();
}
function checkMint(info: AccountInfo<Buffer> | null, token: SolanaToken) {
  const mint = unpackMint(new PublicKey(token.mint), info, new PublicKey(token.program));
  if (!mint.isInitialized || mint.decimals !== token.decimals) throw new Error('Token identity changed.');
  const hook = getTransferHook(mint);
  const scaled = getScaledUiAmountConfig(mint);
  if ((hook && !hook.programId.equals(PublicKey.default)) || getPausableConfig(mint)?.paused ||
    (scaled && (scaled.multiplier !== 1 || scaled.newMultiplier !== 1)))
    throw new Error('Unsupported active Token2022 transfer hook, pause or scaled amount.');
  return mint;
}
/** Current Anchor IDL read from C88XWfp26heEmDkmfSzeXP7Fd7GQJ2j9dDTUsyiZbUTa, 2026-11-14. */
function checkSwapInstruction(ix: TransactionInstruction, intent: JupiterIntent, out: bigint, input: SolanaToken, output: SolanaToken) {
  if (ix.programId.toBase58() !== JUPITER_PROGRAM) throw new Error('Unsupported Jupiter program.');
  const discriminator = Buffer.from(ix.data.subarray(0, 8)).toString('hex');
  const shared = discriminator === 'd19853937cfed8e9';
  if (!shared && discriminator !== 'bb64facc31c4af14') throw new Error('Unsupported Jupiter instruction version.');
  const offset = shared ? 9 : 8;
  if (ix.data.length < offset + 26 || ix.data.readBigUInt64LE(offset) !== BigInt(intent.amount) ||
      ix.data.readBigUInt64LE(offset + 8) !== out || ix.data.readUInt16LE(offset + 16) !== intent.slippageBps ||
      ix.data.readUInt16LE(offset + 18) !== intent.feeBps || ix.data.readUInt16LE(offset + 20) !== 0)
    throw new Error('Jupiter instruction changed the reviewed amounts, slippage or fee.');
  const expected = shared
    ? [[1, intent.owner], [2, ata(intent.owner, input.mint, input.program)], [5, ata(intent.owner, output.mint, output.program)], [6, input.mint], [7, output.mint], [8, input.program], [9, output.program]]
    : [[0, intent.owner], [1, ata(intent.owner, input.mint, input.program)], [2, ata(intent.owner, output.mint, output.program)], [3, input.mint], [4, output.mint], [5, input.program], [6, output.program]];
  for (const [index, address] of expected) {
    if (typeof index !== 'number' || typeof address !== 'string' || ix.keys[index]?.pubkey.toBase58() !== address) throw new Error('Jupiter instruction changed an account.');
  }
  if (intent.feeAccount && !ix.keys.some(key => key.pubkey.toBase58() === intent.feeAccount && key.isWritable)) throw new Error('Missing reviewed fee account.');
}
function tokenAccount(info: AccountInfo<Buffer> | null, address: PublicKey) {
  if (!info || (!info.owner.equals(TOKEN_PROGRAM_ID) && !info.owner.equals(TOKEN_2022_PROGRAM_ID))) return null;
  try { return unpackAccount(address, info, info.owner); } catch { return null; }
}
function unchangedAuthority(a: ReturnType<typeof unpackAccount>, b: ReturnType<typeof unpackAccount>) {
  return a.owner.equals(b.owner) && a.mint.equals(b.mint) && a.delegate?.toBase58() === b.delegate?.toBase58() && a.delegatedAmount === b.delegatedAmount && a.closeAuthority?.toBase58() === b.closeAuthority?.toBase58() && a.isFrozen === b.isFrozen && a.isNative === b.isNative && a.tlvData.equals(b.tlvData);
}
function safeLamports(value: number) { if (!Number.isSafeInteger(value) || value < 0) throw new Error('Unsafe account balance.'); return BigInt(value); }

/** Build locally from raw Jupiter V2 instructions. RPC and Jupiter are explicitly selected by the app. */
export async function prepareJupiterSwap(options: { build: unknown; intent: JupiterIntent; rpc: SolanaRpc; tokens?: readonly SolanaToken[]; now?: number; guard?: SolanaGuard }): Promise<PreparedSolanaSwap> {
  const guard = options.guard ?? { assertCurrent() {} };
  const check = () => { guard.assertCurrent(); guard.signal?.throwIfAborted(); };
  check();
  const intent = JupiterIntentSchema.parse(options.intent), build = JupiterBuildSchema.parse(options.build);
  const input = tokenFor(intent.inputMint, options.tokens ?? SOLANA_TOKENS), output = tokenFor(intent.outputMint, options.tokens ?? SOLANA_TOKENS);
  if (build.inputMint !== intent.inputMint || build.outputMint !== intent.outputMint || build.inAmount !== intent.amount || build.slippageBps !== intent.slippageBps || BigInt(build.outAmount) <= 0n || build.otherInstructions.length || build.tipInstruction)
    throw new Error('Jupiter changed the requested route or added instructions.');
  const minOut = (BigInt(build.outAmount) * BigInt(10000 - intent.slippageBps) + 9999n) / 10000n;
  if (minOut <= 0n || BigInt(build.otherAmountThreshold) !== minOut) throw new Error('Jupiter minimum output changed.');
  const swap = instruction(build.swapInstruction);
  checkSwapInstruction(swap, intent, BigInt(build.outAmount), input, output);
  const allowedAtas = new Map<string, { mint: string; program: string }>();
  for (const token of [input, output]) allowedAtas.set(ata(intent.owner, token.mint, token.program), token);
  const setup = build.setupInstructions.map(instruction);
  const cleanup = build.cleanupInstruction ? instruction(build.cleanupInstruction) : null;
  const compute = build.computeBudgetInstructions.map(instruction);
  let priorityPrice = 0n;
  for (const ix of compute) {
    if (!ix.programId.equals(ComputeBudgetProgram.programId) || ix.keys.length || ix.data.length !== 9 || ix.data[0] !== 3 || priorityPrice !== 0n) throw new Error('Unexpected compute budget instruction.');
    priorityPrice = BigInt(ix.data.readBigUInt64LE(1).toString());
  }
  if ((priorityPrice * 1400000n + 999999n) / 1000000n > BigInt(intent.maxPriorityFeeLamports)) throw new Error('Priority fee exceeds approved ceiling.');
  let nativeTransfers = 0;
  for (const ix of setup) {
    if (ix.programId.equals(ASSOCIATED_TOKEN_PROGRAM_ID)) {
      const account = ix.keys[1]?.pubkey.toBase58(), mint = ix.keys[3]?.pubkey.toBase58(), program = ix.keys[5]?.pubkey.toBase58();
      // Intermediate ATAs may be needed by multihop routes; they are owner-bound and inspected after simulation.
      if (ix.data.length !== 1 || ix.data[0] !== 1 || ix.keys.length !== 6 || ix.keys[0]?.pubkey.toBase58() !== intent.payer || ix.keys[2]?.pubkey.toBase58() !== intent.owner || !mint || !program || ![TOKEN_PROGRAM_ID.toBase58(), TOKEN_2022_PROGRAM_ID.toBase58()].includes(program) || ix.keys[4]?.pubkey.toBase58() !== SystemProgram.programId.toBase58() || account !== ata(intent.owner, mint, program)) throw new Error('Unexpected ATA creation.');
      allowedAtas.set(account, { mint, program });
    } else if (ix.programId.equals(SystemProgram.programId)) {
      nativeTransfers++;
      if (input.mint !== SOL_MINT || ix.data.length !== 12 || ix.data.readUInt32LE(0) !== 2 || ix.data.readBigUInt64LE(4) !== BigInt(intent.amount) || ix.keys.length !== 2 || ix.keys[0]?.pubkey.toBase58() !== intent.owner || ix.keys[1]?.pubkey.toBase58() !== ata(intent.owner, SOL_MINT, input.program)) throw new Error('Unexpected native transfer.');
    } else if (ix.programId.equals(TOKEN_PROGRAM_ID)) {
      if (input.mint !== SOL_MINT || ix.data.length !== 1 || ix.data[0] !== 17 || ix.keys.length !== 1 || ix.keys[0]?.pubkey.toBase58() !== ata(intent.owner, SOL_MINT, input.program)) throw new Error('Unexpected token setup.');
    } else throw new Error('Unsupported setup program.');
  }
  if (nativeTransfers !== (input.mint === SOL_MINT ? 1 : 0)) throw new Error('Unexpected native input funding count.');
  if (cleanup && (!cleanup.programId.equals(TOKEN_PROGRAM_ID) || cleanup.data.length !== 1 || cleanup.data[0] !== 9 || cleanup.keys.length !== 3 || cleanup.keys[0]?.pubkey.toBase58() !== ata(intent.owner, SOL_MINT, TOKEN_PROGRAM_ID.toBase58()) || cleanup.keys[1]?.pubkey.toBase58() !== intent.owner || cleanup.keys[2]?.pubkey.toBase58() !== intent.owner || (input.mint !== SOL_MINT && output.mint !== SOL_MINT))) throw new Error('Unexpected token closure.');
  const tables = [];
  for (const [address, addresses] of Object.entries(build.addressesByLookupTableAddress ?? {})) {
    const table = (await options.rpc.getAddressLookupTable(new PublicKey(address), { commitment: 'confirmed' })).value;
    if (!table || !table.isActive() || table.state.addresses.length < addresses.length || addresses.some((value, index) => table.state.addresses[index]?.toBase58() !== value)) throw new Error('Lookup table changed.');
    tables.push(table);
  }
  check();
  const tx = new VersionedTransaction(new TransactionMessage({ payerKey: new PublicKey(intent.payer), recentBlockhash: bs58.encode(Uint8Array.from(build.blockhashWithMetadata.blockhash)), instructions: [ComputeBudgetProgram.setComputeUnitLimit({ units: 1400000 }), ...compute, ...setup, swap, ...(cleanup ? [cleanup] : [])] }).compileToV0Message(tables));
  const signers = tx.message.staticAccountKeys.slice(0, tx.message.header.numRequiredSignatures).map(key => key.toBase58()).sort();
  if (JSON.stringify(signers) !== JSON.stringify([...new Set([intent.owner, intent.payer])].sort())) throw new Error('Unexpected transaction signer.');
  const keys = tx.message.getAccountKeys({ addressLookupTableAccounts: tables });
  const writable = [...Array(keys.length).keys()].filter(index => tx.message.isAccountWritable(index)).map(index => { const key = keys.get(index); if (!key) throw new Error('Unresolved account.'); return key; });
  if (writable.length > 64) throw new Error('Too many writable swap accounts.');
  const before = await options.rpc.getMultipleAccountsInfo(writable, 'confirmed');
  const mintInfos = await options.rpc.getMultipleAccountsInfo([new PublicKey(input.mint), new PublicKey(output.mint)], 'confirmed');
  if (input.mint !== SOL_MINT) checkMint(mintInfos[0] ?? null, input);
  if (output.mint !== SOL_MINT) checkMint(mintInfos[1] ?? null, output);
  const feeInfo = intent.feeAccount ? (await options.rpc.getMultipleAccountsInfo([new PublicKey(intent.feeAccount)], 'confirmed'))[0] : null;
  if (intent.feeAccount) {
    const fee = tokenAccount(feeInfo ?? null, new PublicKey(intent.feeAccount));
    if (!fee || fee.mint.toBase58() !== intent.feeMint || feeInfo?.owner.toBase58() !== tokenFor(intent.feeMint ?? '', options.tokens ?? SOLANA_TOKENS).program || fee.owner.toBase58() === intent.owner || fee.isFrozen) throw new Error('Invalid integrator fee account.');
  }
  check();
  const fee = (await options.rpc.getFeeForMessage(tx.message, 'confirmed')).value;
  if (fee === null || safeLamports(fee) > BigInt(intent.maxSponsorLamports)) throw new Error('Network fee exceeds approved ceiling.');
  const simulation = await options.rpc.simulateTransaction(tx, { sigVerify: false, replaceRecentBlockhash: false, commitment: 'confirmed', accounts: { encoding: 'base64', addresses: writable.map(key => key.toBase58()) } });
  if (!Number.isSafeInteger(simulation.context.slot) || simulation.context.slot < 0) throw new Error('Invalid simulation slot.');
  if (simulation.value.err || !simulation.value.accounts || simulation.value.accounts.length !== writable.length) throw new Error('Swap simulation failed.');
  let inputChange = 0n, outputChange = 0n, ownerLamports = 0n, sponsorLamports = 0n, feeChange = 0n;
  for (let index = 0; index < writable.length; index++) {
    const key = writable[index]; if (!key) throw new Error('Missing account.');
    const pre = before[index] ?? null, raw = simulation.value.accounts[index];
    const post = raw ? { ...raw, owner: new PublicKey(raw.owner), data: Buffer.from(raw.data[0], 'base64') } : null;
    const preToken = tokenAccount(pre, key), postToken = tokenAccount(post, key);
    const delta = safeLamports(post?.lamports ?? 0) - safeLamports(pre?.lamports ?? 0);
    if (key.toBase58() === intent.owner) ownerLamports += delta;
    if (key.toBase58() === intent.payer) sponsorLamports += delta;
    if (preToken?.owner.toBase58() === intent.payer && intent.payer !== intent.owner && (!postToken || !unchangedAuthority(preToken, postToken) || preToken.amount !== postToken.amount)) throw new Error('Sponsor token account changed.');
    if (preToken?.owner.toBase58() === intent.owner) {
      if (!postToken) {
        if (!cleanup || key.toBase58() !== ata(intent.owner, SOL_MINT, TOKEN_PROGRAM_ID.toBase58())) throw new Error('Existing token account was closed.');
      } else if (!unchangedAuthority(preToken, postToken)) throw new Error('Token authority or extensions changed.');
    }
    if (!pre && postToken?.owner.toBase58() === intent.owner && !allowedAtas.has(key.toBase58())) throw new Error('Unexpected new owner token account.');
    if (postToken?.owner.toBase58() === intent.owner || preToken?.owner.toBase58() === intent.owner) {
      const mint = postToken?.mint.toBase58() ?? preToken?.mint.toBase58();
      const change = (postToken?.amount ?? 0n) - (preToken?.amount ?? 0n);
      if (mint === input.mint) inputChange += change;
      else if (mint === output.mint) outputChange += change;
      else if (change !== 0n) throw new Error('Unrelated wallet token balance changed.');
    }
    if (key.toBase58() === intent.feeAccount) {
      if (!preToken || !postToken || !unchangedAuthority(preToken, postToken)) throw new Error('Fee account changed.');
      feeChange = postToken.amount - preToken.amount;
    }
  }
  if (input.mint === SOL_MINT) {
    // Exact native input is bound both by the System transfer and Jupiter args.
    // A newly sponsored temporary wSOL account may return its rent to the owner.
    if (ownerLamports < -BigInt(intent.amount)) throw new Error('Unreviewed native debit.');
    inputChange = -BigInt(intent.amount);
  }
  if (output.mint === SOL_MINT) outputChange = ownerLamports;
  if (inputChange !== -BigInt(intent.amount) || outputChange < minOut || (input.mint !== SOL_MINT && output.mint !== SOL_MINT && ownerLamports < 0n)) throw new Error('Simulated wallet changes differ from the trade.');
  if (intent.payer !== intent.owner && (-sponsorLamports > BigInt(intent.maxSponsorLamports) || sponsorLamports > 0n)) throw new Error('Sponsor charge exceeds approved ceiling.');
  const feeBase = intent.feeMint === input.mint ? BigInt(intent.amount) : outputChange;
  const feeDenominator = BigInt(intent.feeMint === input.mint ? 10000 : 10000 - intent.feeBps);
  if (intent.feeBps && (feeChange <= 0n || feeChange > (feeBase * BigInt(intent.feeBps) + feeDenominator - 1n) / feeDenominator + 1n)) throw new Error('Integrator fee differs from reviewed terms.');
  check();
  const validity = await options.rpc.isBlockhashValid(tx.message.recentBlockhash, { commitment: 'confirmed', minContextSlot: simulation.context.slot });
  if (validity.value !== true || !Number.isSafeInteger(validity.context.slot) || validity.context.slot < simulation.context.slot) throw new Error('Swap blockhash expired or could not be verified.');
  // A currently known hash cannot expire later than the same RPC's newer latest
  // hash. Keep the signed message unchanged and save that conservative RPC bound
  // separately from Jupiter's signing-eligibility metadata.
  const latest = await options.rpc.getLatestBlockhashAndContext({ commitment: 'confirmed', minContextSlot: validity.context.slot });
  const conservativeExpiryBlockHeight = latest.value.lastValidBlockHeight;
  if (!Number.isSafeInteger(latest.context.slot) || latest.context.slot < validity.context.slot || !Number.isSafeInteger(conservativeExpiryBlockHeight) || conservativeExpiryBlockHeight < 0 || !SolanaAddressSchema.safeParse(latest.value.blockhash).success) throw new Error('RPC blockhash expiry could not be verified.');
  const currentBlockHeight = await options.rpc.getBlockHeight('confirmed');
  if (currentBlockHeight >= build.blockhashWithMetadata.lastValidBlockHeight || currentBlockHeight >= conservativeExpiryBlockHeight) throw new Error('Swap blockhash expired.');
  check();
  return deepFreeze({ version: 1, build, transaction: Buffer.from(tx.serialize()).toString('base64'), message: Buffer.from(tx.message.serialize()).toString('base64'), review: Object.freeze({ ...intent, minimumOutputAmount: minOut.toString(), expectedOutputAmount: build.outAmount, lastValidBlockHeight: build.blockhashWithMetadata.lastValidBlockHeight, conservativeExpiryBlockHeight, preparedAtSlot: simulation.context.slot, expiresAt: (options.now ?? Date.now()) + 60000 }) });
}

/** This checks saved server/local reviewed bytes, never an untrusted caller-supplied prepared record. */
export function validateSignedSolanaSwap(prepared: PreparedSolanaSwap, transaction: string, options: { requireOwnerSignature: boolean; requirePayerSignature?: boolean } = { requireOwnerSignature: true }): VersionedTransaction {
  prepared = PreparedSolanaSwapSchema.parse(prepared);
  const tx = VersionedTransaction.deserialize(Buffer.from(transaction, 'base64'));
  const message = tx.message.serialize();
  if (Buffer.from(message).toString('base64') !== prepared.message) throw new Error('Signed swap changed the reviewed transaction.');
  const original = VersionedTransaction.deserialize(Buffer.from(prepared.transaction, 'base64'));
  if (Buffer.from(original.message.serialize()).toString('base64') !== prepared.message) throw new Error('Corrupt prepared swap.');
  const required = tx.message.staticAccountKeys.slice(0, tx.message.header.numRequiredSignatures).map(key => key.toBase58());
  if (tx.message.staticAccountKeys[0]?.toBase58() !== prepared.review.payer || JSON.stringify([...required].sort()) !== JSON.stringify([...new Set([prepared.review.owner, prepared.review.payer])].sort())) throw new Error('Wrong swap signer or payer.');
  required.forEach((owner, index) => {
    const signature = tx.signatures[index];
    if (!signature) throw new Error('Missing signature slot.');
    const isRequired = (owner === prepared.review.owner && options.requireOwnerSignature) || (owner === prepared.review.payer && options.requirePayerSignature);
    if ((isRequired || signature.some(value => value !== 0)) && !verifySolanaSignature(owner, message, signature)) throw new Error('Invalid swap signature.');
  });
  return tx;
}
export function coSignSolanaSwap(prepared: PreparedSolanaSwap, transaction: string, payer: Keypair): string {
  if (payer.publicKey.toBase58() !== prepared.review.payer) throw new Error('Wrong sponsor.');
  const tx = validateSignedSolanaSwap(prepared, transaction);
  tx.sign([payer]);
  const signed = Buffer.from(tx.serialize()).toString('base64');
  validateSignedSolanaSwap(prepared, signed, { requireOwnerSignature: true, requirePayerSignature: true });
  return signed;
}
export async function signPreparedSolanaSwap(prepared: PreparedSolanaSwap, options: { guard: SolanaGuard; rpc: SolanaRpc; tokens?: readonly SolanaToken[]; withSigner<T>(run: (signer: Keypair) => Promise<T>): Promise<T> }): Promise<string> {
  prepared = deepFreeze(PreparedSolanaSwapSchema.parse(prepared));
  options.guard.assertCurrent(); options.guard.signal?.throwIfAborted();
  if (Date.now() >= prepared.review.expiresAt || await options.rpc.getBlockHeight('confirmed') >= prepared.review.conservativeExpiryBlockHeight) throw new Error('Swap quote expired.');
  const { minimumOutputAmount, expectedOutputAmount, lastValidBlockHeight, conservativeExpiryBlockHeight, preparedAtSlot, expiresAt, ...intent } = prepared.review;
  const verified = await prepareJupiterSwap({ build: prepared.build, intent, rpc: options.rpc, tokens: options.tokens, guard: options.guard });
  if (verified.message !== prepared.message || verified.review.minimumOutputAmount !== minimumOutputAmount || verified.review.expectedOutputAmount !== expectedOutputAmount || verified.review.lastValidBlockHeight !== lastValidBlockHeight || verified.review.preparedAtSlot < preparedAtSlot) throw new Error('Prepared swap terms changed.');
  return options.withSigner(async signer => {
    options.guard.assertCurrent(); options.guard.signal?.throwIfAborted();
    if (signer.publicKey.toBase58() !== prepared.review.owner) throw new Error('Wrong selected Solana wallet.');
    const tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false });
    // Unlock may have taken time; never use the conservative recovery bound to
    // extend the original quote's signing eligibility.
    const currentValidity = await options.rpc.isBlockhashValid(tx.message.recentBlockhash, { commitment: 'confirmed', minContextSlot: verified.review.preparedAtSlot });
    const currentHeight = await options.rpc.getBlockHeight('confirmed');
    options.guard.assertCurrent(); options.guard.signal?.throwIfAborted();
    if (currentValidity.value !== true || currentHeight >= lastValidBlockHeight || currentHeight >= conservativeExpiryBlockHeight || Date.now() >= expiresAt) throw new Error('Swap quote expired.');
    tx.sign([signer]);
    options.guard.assertCurrent(); options.guard.signal?.throwIfAborted();
    if (Date.now() >= prepared.review.expiresAt) throw new Error('Swap quote expired.');
    const signed = Buffer.from(tx.serialize()).toString('base64');
    validateSignedSolanaSwap(prepared, signed);
    return signed;
  });
}
export function solanaTransactionSignature(transaction: string): string {
  const tx = VersionedTransaction.deserialize(Buffer.from(transaction, 'base64'));
  const signature = tx.signatures[0];
  if (!signature || !signature.some(value => value !== 0)) throw new Error('Fee payer has not signed.');
  return bs58.encode(signature);
}
export function jupiterBuildParameters(intent: JupiterIntent): URLSearchParams {
  const value = JupiterIntentSchema.parse(intent);
  return new URLSearchParams({ inputMint: value.inputMint, outputMint: value.outputMint, amount: value.amount, taker: value.owner, payer: value.payer, slippageBps: String(value.slippageBps), platformFeeBps: String(value.feeBps), ...(value.feeAccount ? { feeAccount: value.feeAccount } : {}), transactionVersion: '0' });
}
export function createJupiterBuildAdapter(options: { rpc: SolanaRpc; requestBuild(params: URLSearchParams, signal?: AbortSignal): Promise<unknown>; tokens?: readonly SolanaToken[] }) {
  return Object.freeze({ async prepare(intent: JupiterIntent, guard?: SolanaGuard) { return prepareJupiterSwap({ build: await options.requestBuild(jupiterBuildParameters(intent), guard?.signal), intent, rpc: options.rpc, tokens: options.tokens, guard }); } });
}
