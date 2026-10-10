import { classifyHistory, formatZatoshis, parseZecToZatoshis, WalletError, type HistoryEntry, type WalletSnapshot } from "@z-stack/sdk";

export function pendingFunds(snapshot: WalletSnapshot): bigint {
  return snapshot.balance.totalPending !== undefined
    ? BigInt(snapshot.balance.totalPending) : parseZecToZatoshis(snapshot.balance.pendingZec ?? "0");
}

export function confirmationLabel(entry: HistoryEntry, snapshot: WalletSnapshot): string {
  if (entry.status === "expired") return "Expired";
  if (entry.status !== "mined") return "Pending";
  const count = entry.confirmations ?? (entry.minedHeight !== null && snapshot.scannedHeight !== undefined
    ? Math.max(0, snapshot.scannedHeight - entry.minedHeight + 1) : null);
  if (count === null) return "Mined · confirmation count unavailable";
  const needed = snapshot.confirmations?.untrusted;
  if (classifyHistory(entry).action === "received" && needed !== undefined && count < needed) {
    return `Confirming · ${count}/${needed} confirmations`;
  }
  return `Mined · ${count} ${count === 1 ? "confirmation" : "confirmations"}`;
}

export function loadedWalletStatus(snapshot: WalletSnapshot): string {
  return snapshot.scannedHeight !== undefined && snapshot.scannedHeight >= snapshot.birthdayHeight
    ? `Wallet opened · scanned through block ${snapshot.scannedHeight.toLocaleString()}. Sync to check for new blocks.`
    : "Wallet opened · not scanned yet. Sync to find activity.";
}

// Compact history gives account movement; only full transaction details give
// exact fees and distinguish a small external send from a fee-only self send.
export function activityMovement(entry: HistoryEntry, unit: string): string {
  const item = classifyHistory(entry);
  if (!entry.historyMetadataComplete && entry.feeZat == null && entry.spentZat > 0) {
    const sign = item.netZat < 0n ? "−" : item.netZat > 0n ? "+" : "";
    const amount = item.netZat < 0n ? -item.netZat : item.netZat;
    return `Wallet change · ${sign}${formatZatoshis(amount)} ${unit} · load details for the payment amount`;
  }
  if (item.action === "internal" && item.feeZec) return `Self send · Fee ${item.feeZec} ${unit}`;
  const sign = item.action === "received" ? "+" : item.action === "sent" ? "−" : "";
  return `${item.label} · ${sign}${item.displayZec} ${unit}`;
}

export function paymentErrorMessage(error: unknown, snapshot: WalletSnapshot | undefined, unit: string): string {
  const safe = WalletError.fromUnknown(error);
  if (safe.code === "insufficient_funds" && snapshot && pendingFunds(snapshot) > 0n) {
    const required = snapshot.confirmations?.untrusted;
    const wait = required === undefined ? "" : `. Incoming shielded funds need ${required} ${required === 1 ? "confirmation" : "confirmations"}`;
    return `Not enough available funds for the amount and fee. ${formatZatoshis(pendingFunds(snapshot))} ${unit} is still confirming${wait}. Sync to update.`;
  }
  return safe.userMessage();
}

export function syncedWalletStatus(snapshot: WalletSnapshot): string {
  return `Synced through block ${snapshot.scannedHeight?.toLocaleString() ?? "unknown"}. Sync again for payments mined later.`;
}
