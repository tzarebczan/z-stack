import { parseZip321 } from "@z-stack/core";
import {
  WalletError,
  inspectAddressSummary,
  isTreeConflictError,
  isWalletError,
  treeConflictUserMessage,
  type BalanceEvent,
  type SyncEvent,
  type WalletSnapshot,
  type WasmProgress,
} from "@z-stack/sdk/lab";

export function formatUiError(err: unknown): string {
  const raw = isWalletError(err) ? err.message : err instanceof Error ? err.message : String(err);
  const tree = treeConflictUserMessage(raw);
  if (
    tree === "sync cancelled" ||
    /scan worker restarted/i.test(tree) ||
    isTreeConflictError(tree)
  ) {
    return tree;
  }
  return WalletError.fromUnknown(err).userMessage();
}

export function applyBalanceEvent(
  cur: WalletSnapshot | null,
  e: BalanceEvent,
): WalletSnapshot | null {
  if (!cur) return cur;
  if (e.availableZat === 0 && (cur.balance.totalAvailable ?? 0) > 0) return cur;
  return {
    ...cur,
    balance: {
      ...cur.balance,
      orchardAvailable: e.orchardAvailable ?? cur.balance.orchardAvailable,
      transparentAvailable: e.transparentAvailable ?? cur.balance.transparentAvailable,
      totalAvailable: e.availableZat,
      totalPending: e.pendingZat ?? cur.balance.totalPending,
      totalZec: e.totalZec ?? cur.balance.totalZec,
    },
  };
}

export function syncEventToProgress(e: SyncEvent, cur: WasmProgress | null): WasmProgress {
  const stage =
    e.stage === "connecting" ||
    e.stage === "downloading" ||
    e.stage === "scanning" ||
    e.stage === "enhancing" ||
    e.stage === "synced"
      ? e.stage
      : (cur?.stage ?? "connecting");
  return {
    stage,
    heading: e.heading ?? cur?.heading,
    message: e.message ?? cur?.message,
    scanned: e.scanned ?? cur?.scanned ?? 0,
    downloaded: e.downloaded ?? cur?.downloaded,
    tip: e.tip ?? cur?.tip ?? 0,
    percent: e.percent ?? cur?.percent ?? 0,
    notesFound: e.notesFound ?? cur?.notesFound ?? 0,
    spendsFound: e.spendsFound ?? cur?.spendsFound ?? 0,
    blocksPerSecond: e.blocksPerSecond ?? cur?.blocksPerSecond,
    remainingSeconds: e.remainingSeconds ?? cur?.remainingSeconds,
    remainingHuman: e.remainingHuman ?? cur?.remainingHuman,
    availableZat: cur?.availableZat,
    availableZec: cur?.availableZec,
    orchardAvailable: cur?.orchardAvailable,
    pendingZat: cur?.pendingZat,
  };
}

export function inspectTarget(to: string): string {
  const t = to.trim();
  if (t.toLowerCase().startsWith("zcash:")) {
    try {
      return parseZip321(t).address || t;
    } catch {
      return t;
    }
  }
  return t;
}
