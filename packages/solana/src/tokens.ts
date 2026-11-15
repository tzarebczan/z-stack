import { PublicKey } from '@solana/web3.js';
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID } from '@solana/spl-token';
import { z } from 'zod';
export const SolanaAddressSchema = z.string().refine(value => {
  try { return new PublicKey(value).toBase58() === value; } catch { return false; }
}, 'Invalid Solana address');
export const AtomicAmountSchema = z.string().regex(/^(0|[1-9][0-9]{0,19})$/).refine(value => BigInt(value) <= 0xffffffffffffffffn);
export const SolanaTokenSchema = z.object({
  symbol: z.string().min(1).max(16), mint: SolanaAddressSchema,
  program: z.enum([TOKEN_PROGRAM_ID.toBase58(), TOKEN_2022_PROGRAM_ID.toBase58()]),
  decimals: z.number().int().min(0).max(18),
}).strict();
export type SolanaToken = z.infer<typeof SolanaTokenSchema>;
export const SOL_MINT = 'So11111111111111111111111111111111111111112';
export const SOLANA_USDC = Object.freeze({ symbol: 'USDC', mint: 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', program: TOKEN_PROGRAM_ID.toBase58(), decimals: 6 });
export const SOLANA_CYPH = Object.freeze({ symbol: 'CYPH', mint: 'CYPHuMmCL1GxJWa2tsPhLKykC7GrHJTCHwbXD4g5uawK', program: TOKEN_2022_PROGRAM_ID.toBase58(), decimals: 6 });
export const SOLANA_TOKENS: readonly SolanaToken[] = Object.freeze([SOLANA_USDC, SOLANA_CYPH]);
