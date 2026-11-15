import { Connection, PublicKey, type FetchFn } from '@solana/web3.js';
import { getScaledUiAmountConfig, unpackAccount, unpackMint } from '@solana/spl-token';
import { AtomicAmountSchema, SolanaAddressSchema, SolanaTokenSchema, SOLANA_TOKENS, type SolanaToken } from './tokens';
export type SolanaRpc = Pick<Connection, 'getBalance' | 'getTokenAccountsByOwner' | 'getMultipleAccountsInfo' | 'getAddressLookupTable' | 'simulateTransaction' | 'getFeeForMessage' | 'getBlockHeight' | 'getSignatureStatuses' | 'getTransaction' | 'sendRawTransaction' | 'getFirstAvailableBlock' | 'getBlocks' | 'getBlockSignatures' | 'isBlockhashValid' | 'getBlock' | 'getLatestBlockhashAndContext'>;
export function createSolanaRpcClient(endpoint: string, options: { fetch?: FetchFn } = {}): SolanaRpc {
  const url = new URL(endpoint);
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))) throw new Error('Solana RPC must use HTTPS or loopback.');
  return new Connection(url.href, { commitment: 'confirmed', disableRetryOnRateLimit: true, ...(options.fetch ? { fetch: options.fetch } : {}) });
}
export interface SolanaGuard { readonly signal?: AbortSignal; assertCurrent(): void }
export function createSolanaWallet(options: { address: string; rpc: SolanaRpc; tokens?: readonly SolanaToken[] }) {
  const address = new PublicKey(SolanaAddressSchema.parse(options.address));
  const tokens = (options.tokens ?? SOLANA_TOKENS).map(token => Object.freeze(SolanaTokenSchema.parse(token)));
  return Object.freeze({
    address: address.toBase58(),
    async getBalances(guard: SolanaGuard = { assertCurrent() {} }) {
      guard.assertCurrent(); guard.signal?.throwIfAborted();
      const sol = await options.rpc.getBalance(address, 'confirmed');
      if (!Number.isSafeInteger(sol) || sol < 0) throw new Error('Unsafe SOL balance.');
      const mints = await options.rpc.getMultipleAccountsInfo(tokens.map(token => new PublicKey(token.mint)), 'confirmed');
      const balances = [];
      for (const program of new Set(tokens.map(token => token.program))) {
        const accounts = await options.rpc.getTokenAccountsByOwner(address, { programId: new PublicKey(program) }, 'confirmed');
        for (const token of tokens.filter(value => value.program === program)) {
          const mintInfo = mints[tokens.indexOf(token)];
          if (!mintInfo) throw new Error('Token mint is unavailable.');
          const mint = unpackMint(new PublicKey(token.mint), mintInfo, new PublicKey(program));
          if (mint.decimals !== token.decimals || !mint.isInitialized) throw new Error('Token identity changed.');
          const scaled = getScaledUiAmountConfig(mint);
          let amount = 0n;
          for (const item of accounts.value) {
            const account = unpackAccount(item.pubkey, item.account, new PublicKey(program));
            if (account.owner.equals(address) && account.mint.toBase58() === token.mint) amount += account.amount;
          }
          balances.push(Object.freeze({ ...token, amount: amount.toString(), scaledUiAmount: scaled ? { multiplier: scaled.multiplier, newMultiplier: scaled.newMultiplier, effectiveTimestamp: scaled.newMultiplierEffectiveTimestamp.toString() } : null }));
        }
      }
      guard.assertCurrent(); guard.signal?.throwIfAborted();
      return Object.freeze({ sol: String(sol), tokens: Object.freeze(balances) });
    },
    getStatus(signature: string, lastValidBlockHeight: number) { return getSolanaTransactionStatus(options.rpc, signature, lastValidBlockHeight); },
  });
}
export type SolanaTransactionStatus = { kind: 'confirmed' | 'finalized'; signature: string; slot: number } | { kind: 'failed'; signature: string } | { kind: 'pending' | 'expired-unresolved'; signature: string };
/** Expired without a returned status remains unresolved; this never authorizes a replacement spend. */
export async function getSolanaTransactionStatus(rpc: Pick<SolanaRpc, 'getSignatureStatuses' | 'getBlockHeight'>, signature: string, lastValidBlockHeight: number): Promise<SolanaTransactionStatus> {
  const result = (await rpc.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0];
  if (result?.err) return { kind: 'failed', signature };
  if (result?.confirmationStatus === 'confirmed' || result?.confirmationStatus === 'finalized') return { kind: result.confirmationStatus, signature, slot: result.slot };
  return { kind: await rpc.getBlockHeight('finalized') > lastValidBlockHeight ? 'expired-unresolved' : 'pending', signature };
}

/** Confirm an actual finalized receive delta. The caller must independently bind this signature to its provider's payout record. */
export async function verifySolanaReceive(options: { rpc: SolanaRpc; signature: string; owner: string; mint: string; minimumAmount: string }) {
  const owner = SolanaAddressSchema.parse(options.owner), mint = SolanaAddressSchema.parse(options.mint);
  const minimum = BigInt(AtomicAmountSchema.parse(options.minimumAmount));
  const receipt = await options.rpc.getTransaction(options.signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 });
  if (!receipt || !receipt.meta || receipt.meta.err || receipt.transaction.signatures[0] !== options.signature) throw new Error('Finalized receive transaction is unavailable.');
  let amount = 0n;
  if (mint === 'So11111111111111111111111111111111111111112') {
    const keys = receipt.transaction.message.getAccountKeys({ accountKeysFromLookups: receipt.meta.loadedAddresses });
    const index = [...Array(keys.length).keys()].find(index => keys.get(index)?.toBase58() === owner);
    if (index === undefined) throw new Error('Recipient is missing from the transaction.');
    const before = receipt.meta.preBalances[index], after = receipt.meta.postBalances[index];
    if (!Number.isSafeInteger(before) || !Number.isSafeInteger(after) || before === undefined || after === undefined) throw new Error('Unsafe native receive amount.');
    amount = BigInt(after) - BigInt(before);
  } else {
    for (const item of receipt.meta.postTokenBalances ?? []) if (item.owner === owner && item.mint === mint) {
      const previous = receipt.meta.preTokenBalances?.find(value => value.accountIndex === item.accountIndex && value.mint === mint);
      if (previous && previous.owner === undefined) throw new Error('Previous token account owner is missing.');
      amount += BigInt(item.uiTokenAmount.amount);
    }
    for (const item of receipt.meta.preTokenBalances ?? []) if (item.owner === owner && item.mint === mint) amount -= BigInt(item.uiTokenAmount.amount);
  }
  if (amount <= 0n || amount < minimum) throw new Error('Finalized receive amount is below the required amount.');
  return Object.freeze({ signature: options.signature, owner, mint, amount: amount.toString(), slot: receipt.slot, finality: 'finalized' as const });
}
