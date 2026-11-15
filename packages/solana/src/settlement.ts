import { Buffer } from 'buffer';
import bs58 from 'bs58';
import { z } from 'zod';
import { SolanaAddressSchema } from './tokens';
import { PreparedSolanaSwapSchema, validateSignedSolanaSwap, type PreparedSolanaSwap } from './jupiter';
import type { SolanaRpc } from './wallet';

export type SolanaNonInclusionProof = {
  kind: 'finalized-block-signatures'; signature: string; blockhash: string;
  fromSlot: number; throughSlot: number; checkedBlocks: number;
  finalizedBlockHeight: number; lastValidBlockHeight: number; conservativeExpiryBlockHeight: number; throughBlockhash: string; blockhashValid: false;
};
const finalizedBlockHeaderSchema = z.object({ blockhash: SolanaAddressSchema, previousBlockhash: SolanaAddressSchema, parentSlot: z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER), blockHeight: z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER) });

/** RPC-verified complete bounded finalized history, not a cryptographic light-client proof.
 * Unsupported/pruned history, a moving gap, any inclusion or an overlong range remains unknown. */
export async function proveSolanaNotIncluded(options: { rpc: SolanaRpc; prepared: PreparedSolanaSwap; signature: string }): Promise<SolanaNonInclusionProof | null> {
  try {
    const prepared = PreparedSolanaSwapSchema.parse(options.prepared), rpc = options.rpc;
    if (bs58.decode(options.signature).length !== 64 || bs58.encode(bs58.decode(options.signature)) !== options.signature) return null;
    const tx = validateSignedSolanaSwap(prepared, prepared.transaction, { requireOwnerSignature: false });
    const status = (await rpc.getSignatureStatuses([options.signature], { searchTransactionHistory: true })).value[0];
    if (status || await rpc.getTransaction(options.signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })) return null;
    const validity = await rpc.isBlockhashValid(tx.message.recentBlockhash, { commitment: 'finalized' });
    if (validity.value !== false) return null;
    const fromSlot = prepared.review.preparedAtSlot;
    if (!Number.isSafeInteger(validity.context.slot) || validity.context.slot < fromSlot || await rpc.getFirstAvailableBlock() > fromSlot) return null;
    // Resume always inspects the original bounded validity window, including
    // after a reload days later. Newer unrelated blocks do not enlarge the scan.
    const windowEnd = Math.min(fromSlot + 299, validity.context.slot);
    const slots = await rpc.getBlocks(fromSlot, windowEnd, 'finalized');
    if (!slots.length || slots.length > 300 || slots.some((slot, index) => !Number.isSafeInteger(slot) || slot < fromSlot || slot > windowEnd || index > 0 && slot <= (slots[index - 1] ?? slot))) return null;
    const throughSlot = slots.at(-1);
    if (throughSlot === undefined) return null;
    const headerConfig = { commitment: 'finalized', transactionDetails: 'none', rewards: false, maxSupportedTransactionVersion: 0 } as const;
    const endHeader = finalizedBlockHeaderSchema.nullable().parse(await rpc.getBlock(throughSlot, headerConfig));
    if (!endHeader || endHeader.blockHeight === null || !Number.isSafeInteger(endHeader.blockHeight) || endHeader.blockHeight <= prepared.review.conservativeExpiryBlockHeight) return null;
    const blocks = [];
    for (let offset = 0; offset < slots.length; offset += 8) {
      blocks.push(...await Promise.all(slots.slice(offset, offset + 8).map(slot => rpc.getBlockSignatures(slot, 'finalized'))));
    }
    for (let index = 0; index < blocks.length; index++) {
      const block = blocks[index], previous = index ? blocks[index - 1] : null;
      if (!block || !Array.isArray(block.signatures) || block.signatures.length > 8192 || block.signatures.includes(options.signature)) return null;
      if (index === 0 ? block.parentSlot >= fromSlot : !previous || block.parentSlot !== slots[index - 1] || block.previousBlockhash !== previous.blockhash) return null;
    }
    const last = blocks.at(-1);
    if (!last || last.blockhash !== endHeader.blockhash || last.previousBlockhash !== endHeader.previousBlockhash || last.parentSlot !== endHeader.parentSlot) return null;
    // Re-read the exact finalized header instead of substituting today's tip.
    const confirmedHeader = finalizedBlockHeaderSchema.nullable().parse(await rpc.getBlock(throughSlot, headerConfig));
    if (!confirmedHeader || confirmedHeader.blockhash !== endHeader.blockhash || confirmedHeader.previousBlockhash !== endHeader.previousBlockhash || confirmedHeader.parentSlot !== endHeader.parentSlot || confirmedHeader.blockHeight !== endHeader.blockHeight) return null;
    // The exact message hash is independent of annotations such as providerQuoteId.
    if (!Buffer.from(tx.message.serialize()).equals(Buffer.from(prepared.message, 'base64'))) return null;
    return Object.freeze({ kind: 'finalized-block-signatures', signature: options.signature, blockhash: tx.message.recentBlockhash, fromSlot, throughSlot, checkedBlocks: slots.length, finalizedBlockHeight: endHeader.blockHeight, lastValidBlockHeight: prepared.review.lastValidBlockHeight, conservativeExpiryBlockHeight: prepared.review.conservativeExpiryBlockHeight, throughBlockhash: endHeader.blockhash, blockhashValid: false });
  } catch { return null; }
}
