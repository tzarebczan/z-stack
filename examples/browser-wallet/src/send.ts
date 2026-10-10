import { parseZecToZatoshis, formatZatoshis, WalletError, type Wallet, type WalletSnapshot } from "@z-stack/sdk";

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
export class ReviewOutdatedError extends WalletError {
  override readonly name = "ReviewOutdatedError";

  constructor() {
    super("cancelled", "This payment review is out of date.");
  }
}

/** Also call immediately before broadcast: sync and proving can outlast a review. */
export function assertReviewCurrent(review: SendReview): void {
  if (Date.now() - review.reviewedAt >= 5 * 60_000) throw new ReviewOutdatedError();
}

/** Return fresh state so the UI can render it even if payment validation fails. */
export async function syncForReview(wallet: Wallet, review: SendReview, signal?: AbortSignal): Promise<WalletSnapshot> {
  signal?.throwIfAborted();
  assertReviewCurrent(review);
  const cancelSync = () => wallet.cancelSync();
  signal?.addEventListener("abort", cancelSync, { once: true });
  try {
    // Entering the phrase can leave the wallet more than 10 blocks behind.
    const snapshot = await wallet.sync();
    signal?.throwIfAborted();
    return snapshot;
  } finally {
    signal?.removeEventListener("abort", cancelSync);
  }
}

export async function recheckReview(wallet: Wallet, review: SendReview, signal?: AbortSignal): Promise<void> {
  signal?.throwIfAborted();
  assertReviewCurrent(review);
  const current = await reviewSend(wallet, review);
  signal?.throwIfAborted();
  assertReviewCurrent(review);
  if (current.walletAddress !== review.walletAddress || current.feeZat !== review.feeZat) {
    throw new ReviewOutdatedError();
  }
}

export async function refreshReceipt(wallet: Wallet, receipt: SendReceipt): Promise<SendReceipt> {
  const entry = await wallet.transaction(receipt.txid);
  if (entry?.status === "mined" || entry?.status === "expired") return { ...receipt, state: entry.status };
  return receipt;
}
