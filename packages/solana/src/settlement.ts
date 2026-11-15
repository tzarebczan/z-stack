import { Buffer } from 'buffer';
import bs58 from 'bs58';
import { PreparedSolanaSwapSchema, validateSignedSolanaSwap, type PreparedSolanaSwap } from './jupiter';
import type { SolanaRpc } from './wallet';

export type SolanaNonInclusionProof = {
  kind: 'finalized-block-signatures'; signature: string; blockhash: string;
  fromSlot: number; throughSlot: number; checkedBlocks: number;
  finalizedBlockHeight: number; lastValidBlockHeight: number; blockhashValid: false;
};
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
    const fromSlot = prepared.review.preparedAtSlot, throughSlot = validity.context.slot;
    if (!Number.isSafeInteger(throughSlot) || throughSlot < fromSlot || throughSlot - fromSlot + 1 > 300 || await rpc.getFirstAvailableBlock() > fromSlot) return null;
    const finalizedBlockHeight = await rpc.getBlockHeight('finalized');
    if (finalizedBlockHeight <= prepared.review.lastValidBlockHeight) return null;
    const slots = await rpc.getBlocks(fromSlot, throughSlot, 'finalized');
    if (!slots.length || slots.length > 300 || slots.at(-1) !== throughSlot || slots.some((slot, index) => !Number.isSafeInteger(slot) || slot < fromSlot || slot > throughSlot || index > 0 && slot <= (slots[index - 1] ?? slot))) return null;
    const blocks = [];
    for (let offset = 0; offset < slots.length; offset += 8) {
      blocks.push(...await Promise.all(slots.slice(offset, offset + 8).map(slot => rpc.getBlockSignatures(slot, 'finalized'))));
    }
    for (let index = 0; index < blocks.length; index++) {
      const block = blocks[index], previous = index ? blocks[index - 1] : null;
      if (!block || !Array.isArray(block.signatures) || block.signatures.length > 8192 || block.signatures.includes(options.signature)) return null;
      if (index === 0 ? block.parentSlot >= fromSlot : !previous || block.parentSlot !== slots[index - 1] || block.previousBlockhash !== previous.blockhash) return null;
    }
    // The exact message hash is independent of annotations such as providerQuoteId.
    if (!Buffer.from(tx.message.serialize()).equals(Buffer.from(prepared.message, 'base64'))) return null;
    return Object.freeze({ kind: 'finalized-block-signatures', signature: options.signature, blockhash: tx.message.recentBlockhash, fromSlot, throughSlot, checkedBlocks: slots.length, finalizedBlockHeight, lastValidBlockHeight: prepared.review.lastValidBlockHeight, blockhashValid: false });
  } catch { return null; }
}
