/**
 * History helpers adapted from an earlier MIT-licensed wallet library.
 * Copyright (c) 2024 zStealthLabs. See NOTICE and THIRD_PARTY_LICENSES.txt.
 *
 * Classify engine `HistoryEntry` rows:
 * per-pool spent/received → action type + display amount. Never persist `type`.
 *
 * Ironwood is still required so a shield into that pool is not reported as a send.
 */

import type { HistoryEntry, HistoryStatus } from "./index";

const ZAT = 100_000_000n;

function formatZec(zats: bigint): string {
  const neg = zats < 0n;
  const v = neg ? -zats : zats;
  const whole = v / ZAT;
  const frac = (v % ZAT).toString().padStart(8, "0");
  return `${neg ? "-" : ""}${whole}.${frac}`;
}

export type TxAction = "sent" | "received" | "shielding" | "deshielding" | "internal" | "unknown";

export type ClassifiedHistory = {
  txid: string;
  action: TxAction;
  label: string;
  status: HistoryStatus;
  /** Always ≥ 0; the amount shown next to the action (not the signed net). */
  displayZat: bigint;
  displayZec: string;
  /** Signed account delta (received − spent, fee in spent). */
  netZat: bigint;
  feeZat: bigint | null;
  feeZec: string | null;
  memos: string[];
  minedHeight: number | null;
  confirmations: number | null;
  blockTime: number | null;
  isShielding: boolean;
};

const ACTION_LABEL: Record<TxAction, string> = {
  sent: "Sent",
  received: "Received",
  shielding: "Shielding",
  deshielding: "Deshielding",
  internal: "Self send",
  unknown: "Transaction",
};

function zat(n: number | null | undefined): bigint {
  if (n == null || !Number.isFinite(n)) return 0n;
  return BigInt(Math.trunc(n));
}

function pools(entry: HistoryEntry) {
  return {
    transparentSpent: zat(entry.transparentSpent),
    transparentReceived: zat(entry.transparentReceived),
    saplingSpent: zat(entry.saplingSpent),
    saplingReceived: zat(entry.saplingReceived),
    orchardSpent: zat(entry.orchardSpent),
    orchardReceived: zat(entry.orchardReceived),
    ironwoodSpent: zat(entry.ironwoodSpent),
    ironwoodReceived: zat(entry.ironwoodReceived),
  };
}

function anyPool(p: ReturnType<typeof pools>): boolean {
  return (
    p.transparentSpent +
      p.transparentReceived +
      p.saplingSpent +
      p.saplingReceived +
      p.orchardSpent +
      p.orchardReceived +
      p.ironwoodSpent +
      p.ironwoodReceived >
    0n
  );
}

/**
 * Classify from per-pool movement. Every shielded pool is required so a
 * network-upgrade receive is not misread as an ordinary send.
 */
export function inferTransactionType(params: {
  netValue: bigint;
  fee?: bigint;
  transparentSpent: bigint;
  transparentReceived: bigint;
  saplingSpent: bigint;
  saplingReceived: bigint;
  orchardSpent: bigint;
  orchardReceived: bigint;
  ironwoodSpent: bigint;
  ironwoodReceived: bigint;
}): TxAction {
  const {
    netValue,
    fee,
    transparentSpent,
    transparentReceived,
    saplingSpent,
    saplingReceived,
    orchardSpent,
    orchardReceived,
    ironwoodSpent,
    ironwoodReceived,
  } = params;
  const shieldedSpent = saplingSpent + orchardSpent + ironwoodSpent;
  const shieldedReceived = saplingReceived + orchardReceived + ironwoodReceived;
  const totalSpent = transparentSpent + shieldedSpent;
  const totalReceived = transparentReceived + shieldedReceived;
  const feeThreshold = fee && fee > 0n ? fee : 50_000n;

  // A same-pool self transfer with change can have a negative account delta
  // equal to the network fee. Check it before the generic debit branches.
  if (fee && fee > 0n && totalSpent > 0n && totalReceived > 0n
      && totalSpent - totalReceived === fee && transparentReceived === 0n
      && transparentSpent === 0n) return "internal";

  if (netValue < -feeThreshold) return "sent";
  if (netValue > feeThreshold) return "received";

  const isInternalTransfer = (spent: bigint, received: bigint): boolean => {
    const impliedFee = spent - received;
    if (impliedFee <= 0n || impliedFee > 1_000_000n) return false;
    const tolerance = 1000n;
    const expectedNet = -impliedFee;
    return netValue >= expectedNet - tolerance && netValue <= expectedNet + tolerance;
  };

  if (transparentSpent > 0n && shieldedReceived > 0n && shieldedSpent === 0n) {
    if (isInternalTransfer(transparentSpent, totalReceived)) return "shielding";
  }
  if (shieldedSpent > 0n && transparentReceived > 0n && transparentSpent === 0n) {
    if (isInternalTransfer(shieldedSpent, totalReceived)) return "deshielding";
  }
  if (transparentSpent === 0n && transparentReceived === 0n && shieldedSpent > 0n) {
    const poolList = [
      { spent: saplingSpent, received: saplingReceived },
      { spent: orchardSpent, received: orchardReceived },
      { spent: ironwoodSpent, received: ironwoodReceived },
    ];
    const gainedWithoutSpending = poolList.some((p) => p.received > 0n && p.spent === 0n);
    const drewOnMultiplePools = poolList.filter((p) => p.spent > 0n).length > 1;
    if ((gainedWithoutSpending || drewOnMultiplePools) && isInternalTransfer(shieldedSpent, shieldedReceived)) {
      return "internal";
    }
  }

  if (netValue > 0n) return "received";
  if (netValue < 0n) return "sent";
  if (totalSpent > 0n) return "internal";
  return "unknown";
}

export function computeDisplayValue(params: {
  type: TxAction;
  netValue: bigint;
  fee?: bigint;
  transparentSpent: bigint;
  transparentReceived: bigint;
  saplingSpent: bigint;
  saplingReceived: bigint;
  orchardSpent: bigint;
  orchardReceived: bigint;
  ironwoodSpent: bigint;
  ironwoodReceived: bigint;
}): bigint {
  const shieldedReceived = params.saplingReceived + params.orchardReceived + params.ironwoodReceived;
  const totalSpent =
    params.transparentSpent + params.saplingSpent + params.orchardSpent + params.ironwoodSpent;
  const totalReceived = params.transparentReceived + shieldedReceived;
  switch (params.type) {
    case "received":
      return params.netValue > 0n ? params.netValue : 0n;
    case "sent": {
      const sentPlusFee = totalSpent - totalReceived;
      if (params.fee && params.fee > 0n && sentPlusFee > params.fee) return sentPlusFee - params.fee;
      if (sentPlusFee > 0n) return sentPlusFee;
      return params.netValue < 0n ? -params.netValue : params.netValue;
    }
    case "shielding":
      return shieldedReceived;
    case "deshielding":
      return params.transparentReceived;
    case "internal":
      return shieldedReceived > 0n
        ? shieldedReceived
        : params.netValue < 0n
          ? -params.netValue
          : params.netValue;
    default:
      return params.netValue < 0n ? -params.netValue : params.netValue;
  }
}

function impliedFee(entry: HistoryEntry, p: ReturnType<typeof pools>): bigint | undefined {
  if (entry.feeZat != null && entry.feeZat > 0) return zat(entry.feeZat);
  const totalSpent = p.transparentSpent + p.saplingSpent + p.orchardSpent + p.ironwoodSpent;
  const totalReceived = p.transparentReceived + p.saplingReceived + p.orchardReceived + p.ironwoodReceived;
  if (totalSpent <= totalReceived) return undefined;
  const implied = totalSpent - totalReceived;
  const shieldedSpent = p.saplingSpent + p.orchardSpent + p.ironwoodSpent;
  const shieldedReceived = p.saplingReceived + p.orchardReceived + p.ironwoodReceived;
  const isShielding = p.transparentSpent > 0n && shieldedReceived > 0n && shieldedSpent === 0n;
  const isDeshielding = shieldedSpent > 0n && p.transparentReceived > 0n && p.transparentSpent === 0n;
  const isMigrate =
    p.transparentSpent === 0n &&
    p.transparentReceived === 0n &&
    shieldedSpent > 0n &&
    [
      { spent: p.saplingSpent, received: p.saplingReceived },
      { spent: p.orchardSpent, received: p.orchardReceived },
      { spent: p.ironwoodSpent, received: p.ironwoodReceived },
    ].some((x) => x.received > 0n && x.spent === 0n);
  if ((isShielding || isDeshielding || isMigrate) && implied > 0n && implied <= 100_000n) return implied;
  return undefined;
}

function fallbackAction(entry: HistoryEntry): TxAction {
  if (entry.isShielding) return "shielding";
  const net = zat(entry.accountDeltaZat);
  const fee = zat(entry.feeZat);
  const threshold = fee > 0n ? fee : 50_000n;
  if (net > threshold) return "received";
  if (net < -threshold) return "sent";
  if (zat(entry.spentZat) > 0n && zat(entry.receivedZat) > 0n) return "internal";
  if (net > 0n) return "received";
  if (net < 0n) return "sent";
  return "unknown";
}

export function classifyHistory(entry: HistoryEntry): ClassifiedHistory {
  const p = pools(entry);
  // Per-pool receipts include change; older history rows' accountDeltaZat
  // counted only external receipts and overstated the debit of self sends.
  const netValue = anyPool(p)
    ? p.transparentReceived + p.saplingReceived + p.orchardReceived + p.ironwoodReceived
      - p.transparentSpent - p.saplingSpent - p.orchardSpent - p.ironwoodSpent
    : zat(entry.accountDeltaZat);
  const fee = impliedFee(entry, p);
  let action = anyPool(p)
    ? inferTransactionType({ netValue, fee, ...p })
    : fallbackAction(entry);
  if (entry.historyMetadataComplete && fee && fee > 0n
      && zat(entry.spentZat) > 0n && zat(entry.receivedZat) > 0n
      && netValue === -fee && zat(entry.outgoingShieldedZat) === 0n
      && !(entry.transparentOutputs?.length)) action = "internal";
  let displayZat = anyPool(p)
    ? computeDisplayValue({ type: action, netValue, fee, ...p })
    : action === "sent"
      ? (() => {
          const abs = netValue < 0n ? -netValue : netValue;
          return fee && abs > fee ? abs - fee : abs;
        })()
      : action === "shielding"
        ? zat(entry.receivedZat)
        : netValue < 0n
          ? -netValue
          : netValue;

  if (action === "internal" && fee && netValue === -fee) displayZat = 0n;
  if (entry.historyMetadataComplete) {
    if (action === "sent" && zat(entry.outgoingShieldedZat) > 0n) {
      displayZat = zat(entry.outgoingShieldedZat);
    }
  }

  const memos = (entry.memos ?? []).map((m) => m.trim()).filter(Boolean);
  // The transaction's chain fee is not this wallet's fee on an incoming
  // payment. Its sender paid it.
  const feeZat = zat(entry.spentZat) > 0n
    ? fee ?? (entry.feeZat != null ? zat(entry.feeZat) : null)
    : null;
  return {
    txid: entry.txid,
    action,
    label: ACTION_LABEL[action],
    status: entry.status,
    displayZat,
    displayZec: formatZec(displayZat),
    netZat: netValue,
    feeZat: feeZat && feeZat > 0n ? feeZat : null,
    feeZec: feeZat && feeZat > 0n ? formatZec(feeZat) : null,
    memos,
    minedHeight: entry.minedHeight,
    confirmations: entry.confirmations ?? null,
    blockTime: entry.blockTime ?? null,
    isShielding: entry.isShielding || action === "shielding",
  };
}

export function classifyHistoryList(entries: HistoryEntry[]): ClassifiedHistory[] {
  const rows = entries.map(classifyHistory);
  const byTxid = new Map(entries.map((entry, index) => [entry.txid.toLowerCase(), index]));
  for (let targetIndex = 0; targetIndex < entries.length; targetIndex++) {
    const target = entries[targetIndex];
    if (!target.historyMetadataComplete || !target.transparentInputs?.length
        || zat(target.spentZat) !== 0n || zat(target.receivedZat) === 0n) continue;
    const linked = target.transparentInputs.map((input) => {
      const sourceIndex = byTxid.get(input.txid.toLowerCase());
      if (sourceIndex === undefined) return null;
      const source = entries[sourceIndex];
      const output = source.transparentOutputs?.find((o) => o.index === input.index);
      return source.historyMetadataComplete && output
        ? { sourceIndex, value: zat(output.valueZat) } : null;
    });
    if (linked.some((link) => link === null)) continue;
    const inputZat = linked.reduce((sum, link) => sum + (link?.value ?? 0n), 0n);
    const received = zat(target.receivedZat);
    const fee = inputZat - received;
    if (fee < 0n || fee > 1_000_000n) continue;
    const targetRow = rows[targetIndex];
    targetRow.action = "shielding";
    targetRow.label = ACTION_LABEL.shielding;
    targetRow.displayZat = received;
    targetRow.displayZec = formatZec(received);
    targetRow.feeZat = fee > 0n ? fee : null;
    targetRow.feeZec = fee > 0n ? formatZec(fee) : null;
    targetRow.isShielding = true;
    for (const link of linked) {
      const sourceIndex = link!.sourceIndex;
      const source = entries[sourceIndex];
      if (zat(source.spentZat) === 0n) continue;
      const sourceRow = rows[sourceIndex];
      sourceRow.action = "deshielding";
      sourceRow.label = ACTION_LABEL.deshielding;
      const spentToTransparent = (linked.filter((item) => item?.sourceIndex === sourceIndex)
        .reduce((sum, item) => sum + (item?.value ?? 0n), 0n));
      sourceRow.displayZat = spentToTransparent;
      sourceRow.displayZec = formatZec(spentToTransparent);
    }
  }
  return rows;
}

export type HistoryQuery = {
  status?: HistoryStatus;
  txid?: string;
};

/** Status derived the same way as the engine (`mined` / `expired` / `pending`). */
export function historyStatusOf(entry: HistoryEntry): HistoryStatus {
  if (entry.status === "mined" || entry.status === "pending" || entry.status === "expired") {
    return entry.status;
  }
  if (entry.minedHeight != null) return "mined";
  if (entry.expiredUnmined) return "expired";
  return "pending";
}

/** Filter then cap. Used when a caller already holds rows (WASM snapshot fallback). */
export function filterHistory(
  entries: HistoryEntry[],
  query?: HistoryQuery,
  limit = entries.length,
): HistoryEntry[] {
  let rows = entries;
  const txid = query?.txid?.trim().toLowerCase();
  if (txid) rows = rows.filter((e) => e.txid.toLowerCase() === txid);
  if (query?.status) rows = rows.filter((e) => historyStatusOf(e) === query.status);
  const cap = Math.max(0, limit);
  return rows.slice(0, cap);
}

export function historyActionLabel(action: TxAction): string {
  return ACTION_LABEL[action];
}

/** Compact relative time from a unix-seconds block timestamp. */
export function formatHistoryTime(blockTime: number | null | undefined): string {
  if (blockTime == null || blockTime <= 0) return "";
  const ms = blockTime < 41_000_000_000 ? blockTime * 1000 : blockTime;
  const diff = Date.now() - ms;
  const mins = Math.floor(diff / 60_000);
  if (mins < 1) return "Just now";
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days === 1) return "Yesterday";
  if (days < 7) return `${days}d ago`;
  return new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric" });
}
