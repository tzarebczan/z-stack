import { Buffer } from 'buffer';
import { Keypair, PublicKey, SystemProgram, VersionedTransaction } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token';
import { type SolanaRpc, JUPITER_PROGRAM, SOLANA_USDC, SOLANA_CYPH } from '../src/index';
export const owner = Keypair.fromSeed(new Uint8Array(32).fill(1)), payer = Keypair.fromSeed(new Uint8Array(32).fill(2));
const inputMint = new PublicKey(SOLANA_USDC.mint), outputMint = new PublicKey(SOLANA_CYPH.mint);
const inputAta = getAssociatedTokenAddressSync(inputMint, owner.publicKey), outputAta = getAssociatedTokenAddressSync(outputMint, owner.publicKey, false, TOKEN_2022_PROGRAM_ID);
function account(mint: PublicKey, amount: bigint, delegate = PublicKey.default) {
  const data = Buffer.alloc(AccountLayout.span);
  AccountLayout.encode({ mint, owner: owner.publicKey, amount, delegateOption: delegate.equals(PublicKey.default) ? 0 : 1, delegate, state: 1, isNativeOption: 0, isNative: 0n, delegatedAmount: 0n, closeAuthorityOption: 0, closeAuthority: PublicKey.default }, data);
  return { owner: mint.equals(outputMint) ? TOKEN_2022_PROGRAM_ID : TOKEN_PROGRAM_ID, data, executable: false, lamports: 2039280, rentEpoch: 0 };
}
function mint(program = TOKEN_PROGRAM_ID) { const data = Buffer.alloc(MintLayout.span); MintLayout.encode({ mintAuthorityOption: 0, mintAuthority: PublicKey.default, supply: 100000n, decimals: 6, isInitialized: true, freezeAuthorityOption: 0, freezeAuthority: PublicKey.default }, data); return { owner: program, data, executable: false, lamports: 1000000, rentEpoch: 0 }; }
export function fixture() {
  const system = (lamports: number) => ({ owner: SystemProgram.programId, data: Buffer.alloc(0), executable: false, lamports, rentEpoch: 0 });
  const before = new Map([[owner.publicKey.toBase58(), system(10000000)], [payer.publicKey.toBase58(), system(10000000)], [inputAta.toBase58(), account(inputMint, 1000n)], [outputAta.toBase58(), account(outputMint, 0n)], [inputMint.toBase58(), mint()], [outputMint.toBase58(), mint(TOKEN_2022_PROGRAM_ID)]]);
  const after = new Map(before); after.set(payer.publicKey.toBase58(), system(9995000)); after.set(inputAta.toBase58(), account(inputMint, 900n)); after.set(outputAta.toBase58(), account(outputMint, 200n));
  const data = Buffer.alloc(39); Buffer.from('bb64facc31c4af14', 'hex').copy(data); data.writeBigUInt64LE(100n, 8); data.writeBigUInt64LE(200n, 16); data.writeUInt16LE(50, 24); data.writeUInt32LE(1, 30);
  const swap = { programId: JUPITER_PROGRAM, accounts: [owner.publicKey, inputAta, outputAta, inputMint, outputMint, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, new PublicKey(JUPITER_PROGRAM), Keypair.fromSeed(new Uint8Array(32).fill(5)).publicKey, new PublicKey(JUPITER_PROGRAM)].map((pubkey, index) => ({ pubkey: pubkey.toBase58(), isSigner: index === 0, isWritable: index === 1 || index === 2 })), data: data.toString('base64') };
  const build = { inputMint: inputMint.toBase58(), outputMint: outputMint.toBase58(), inAmount: '100', outAmount: '200', otherAmountThreshold: '199', swapMode: 'ExactIn', slippageBps: 50, computeBudgetInstructions: [], setupInstructions: [], swapInstruction: swap, cleanupInstruction: null, otherInstructions: [], tipInstruction: null, addressesByLookupTableAddress: null, blockhashWithMetadata: { blockhash: [...new Uint8Array(32).fill(8)], lastValidBlockHeight: 100 } };
  const rpc = {
    getMultipleAccountsInfo: async (keys: PublicKey[]) => keys.map(key => before.get(key.toBase58()) ?? null),
    getAddressLookupTable: async () => ({ context: { slot: 1 }, value: null }),
    getFeeForMessage: async () => ({ context: { slot: 1 }, value: 5000 }),
    getMinimumBalanceForRentExemption: async (size: number) => (size + 128) * 6960,
    getBlockHeight: async () => 10,
    isBlockhashValid: async () => ({ context: { slot: 1 }, value: true }),
    getLatestBlockhashAndContext: async () => ({ context: { slot: 1 }, value: { blockhash: Keypair.fromSeed(new Uint8Array(32).fill(9)).publicKey.toBase58(), lastValidBlockHeight: 120 } }),
    simulateTransaction: async (_tx: VersionedTransaction, options: { accounts: { addresses: string[] } }) => ({ context: { slot: 1 }, value: { err: null, accounts: options.accounts.addresses.map(address => { const account = after.get(address); return account ? { ...account, owner: account.owner.toBase58(), data: [account.data.toString('base64'), 'base64'] } : null; }) } }),
  } as unknown as SolanaRpc;
  const intent = { owner: owner.publicKey.toBase58(), payer: payer.publicKey.toBase58(), inputMint: inputMint.toBase58(), outputMint: outputMint.toBase58(), amount: '100', slippageBps: 50, feeBps: 0, feeAccount: null, feeMint: null, maxSponsorLamports: '10000000', maxPriorityFeeLamports: '100000' };
  const tokens = [{ symbol: 'IN', mint: inputMint.toBase58(), program: TOKEN_PROGRAM_ID.toBase58(), decimals: 6 }, { symbol: 'OUT', mint: outputMint.toBase58(), program: TOKEN_2022_PROGRAM_ID.toBase58(), decimals: 6 }];
  return { build, rpc, intent, tokens, before, after, account };
}

