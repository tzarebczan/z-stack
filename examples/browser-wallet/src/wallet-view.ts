import { classifyHistory, formatZatoshis, parseZecToZatoshis, WalletError, type HistoryEntry, type SyncEvent, type WalletSnapshot, type Wallet } from "@z-stack/sdk";

export function scannerLabel(runtime: Wallet["runtime"]): string {
  if (runtime.scanner === "starting") return "Starting scanner…";
  if (runtime.scanner === "main-thread") return "Scanner starts when you create or restore";
  return runtime.mode === "multi-thread"
    ? `Threaded scanner · ${runtime.threads} threads` : "Single-thread scanner";
}

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
  if (entry.historyMetadataComplete === false && entry.feeZat == null && entry.spentZat > 0) {
    const sign = item.netZat < 0n ? "−" : item.netZat > 0n ? "+" : "";
    const amount = item.netZat < 0n ? -item.netZat : item.netZat;
    return `Wallet change · ${sign}${formatZatoshis(amount)} ${unit} · load details for the payment amount`;
  }
  if (item.action === "internal" && item.feeZec) return `Self send · Fee ${item.feeZec} ${unit}`;
  const sign = item.action === "received" ? "+" : item.action === "sent" ? "−" : "";
  return `${item.label} · ${sign}${item.displayZec} ${unit}`;
}

export function paymentErrorMessage(error: unknown, snapshot: WalletSnapshot | undefined, unit: string): string {
  if (error instanceof Error && error.name === "ReviewOutdatedError") {
    return "This review is out of date. Choose Edit and review the payment again.";
  }
  const safe = WalletError.fromUnknown(error);
  if (safe.code === "insufficient_funds" && snapshot && pendingFunds(snapshot) > 0n) {
    return `Not enough available funds for the amount and fee. ${formatZatoshis(pendingFunds(snapshot))} ${unit} is still confirming. ${confirmationPolicyText(snapshot.confirmations)}`;
  }
  return safe.userMessage();
}

/** Fixed copy only: progress `message` text can describe provider state. */
export function syncProgressLabel(progress: SyncEvent): string {
  if (progress.stage === "synced") {
    return `Synced through block ${progress.scanned?.toLocaleString() ?? "unknown"}. Sync again for payments mined later.`;
  }
  // load() also reports snapshot hydration through the sync event.
  if (progress.activity === "loading") return "Opening saved wallet…";
  if (progress.activity === "waiting_for_server") return "Light server unreachable · retrying. Cancel sync to stop.";
  return `Syncing · ${Math.round(progress.percent ?? 0)}%`;
}

export function syncedWalletStatus(snapshot: WalletSnapshot): string {
  return `Synced through block ${snapshot.scannedHeight?.toLocaleString() ?? "unknown"}. Sync again for payments mined later.`;
}

export function confirmationPolicyText(policy: WalletSnapshot["confirmations"]): string {
  if (!policy) return "Sync to update.";
  const count = (value: number) => `${value} ${value === 1 ? "confirmation" : "confirmations"}`;
  return policy.trusted === policy.untrusted
    ? `Funds need ${count(policy.untrusted)}. Sync to update.`
    : `Incoming payments need ${count(policy.untrusted)}; your change needs ${count(policy.trusted)}. Sync to update.`;
}

/** Use the full enhancement queue status, not just the visible history page. */
export function memoDetailsMessage(snapshot: WalletSnapshot, entries: HistoryEntry[]): string {
  if (snapshot.memoFetchStatus === "scanning") return "This batch is loaded. Load again for remaining memos and details.";
  if (snapshot.memoFetchStatus === "unsupported") return "This connection cannot load memos and transaction details.";
  if (snapshot.memoFetchStatus === "unavailable" || entries.some(entry => entry.historyMetadataComplete === false)) {
    return "Some details are unavailable. Try again later.";
  }
  return snapshot.memoFetchStatus === "complete" ? "Memos and transaction details loaded."
    : "Recent activity refreshed. Load again to check remaining details.";
}
