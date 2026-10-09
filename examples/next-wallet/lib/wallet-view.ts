import { classifyHistory, parseZecToZatoshis, type HistoryEntry, type WalletSnapshot } from "@z-stack/sdk";

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
