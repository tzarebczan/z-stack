import { parseZecToZatoshis, formatZatoshis, WalletError, type Wallet } from "@z-stack/sdk";

export type SendDraft = Readonly<{ to: string; amount: string; memo: string }>;
export type SendReview = SendDraft & Readonly<{ feeZat: number; walletAddress: string; reviewedAt: number }>;
export type SendReceipt = Readonly<{ txid: string; state: "pending" | "mined" | "unknown" | "expired" }>;

export async function reviewSend(wallet: Wallet, draft: SendDraft): Promise<SendReview> {
  const to = draft.to.trim();
  // This simple example accepts an address. ZIP-321 multi-pay needs its own review UI.
  if (to.toLowerCase().startsWith("zcash:")) throw new WalletError("unsupported_payment_uri", "payment link needs a separate review");
  if (!to) throw new WalletError("invalid_address", "Enter a shielded address.");
  const amount = formatZatoshis(parseZecToZatoshis(draft.amount.trim()));
  if (parseZecToZatoshis(amount) <= 0n) throw new WalletError("invalid_amount", "Enter a positive amount.");
  if (new TextEncoder().encode(draft.memo).byteLength > 512) throw new WalletError("invalid_memo", "Memo exceeds 512 bytes.");
  const fee = await wallet.estimateFee(to, amount, draft.memo || undefined);
  const snapshot = await wallet.getWallet();
  if (parseZecToZatoshis(amount) + BigInt(fee.feeZat) > BigInt(snapshot.balance.totalAvailable)) {
    throw new WalletError("insufficient_funds", "The amount and fee exceed your available balance.");
  }
  return Object.freeze({ to, amount, memo: draft.memo, feeZat: fee.feeZat,
    walletAddress: snapshot.unifiedAddress, reviewedAt: Date.now() });
}

/** App-owned: the SDK's `wallet_changed` copy describes another tab, not a stale review. */
export class ReviewOutdatedError extends Error {
  override readonly name = "ReviewOutdatedError";
}

export async function recheckReview(wallet: Wallet, review: SendReview): Promise<void> {
  if (Date.now() - review.reviewedAt > 5 * 60_000) throw new ReviewOutdatedError("review expired");
  // The SDK refuses a send more than 10 blocks behind the tip; entering the
  // phrase can take that long on a fast chain.
  await wallet.sync();
  const current = await reviewSend(wallet, review);
  if (current.walletAddress !== review.walletAddress || current.feeZat !== review.feeZat) {
    throw new ReviewOutdatedError("wallet or fee changed");
  }
}

export async function refreshReceipt(wallet: Wallet, receipt: SendReceipt): Promise<SendReceipt> {
  const entry = await wallet.transaction(receipt.txid);
  if (entry?.status === "mined" || entry?.status === "expired") return { ...receipt, state: entry.status };
  return receipt;
}
