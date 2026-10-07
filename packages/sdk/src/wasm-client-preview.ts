import {
  formatZatoshis,
  zip321Uri,
  type WalletSnapshot
} from "@z-stack/core";

/** Full JSON.parse of the IndexedDB blob. Bigger wallets use a header peek so the UI thread does not freeze. */
export const PEEK_FULL_PARSE_MAX = 4 * 1024 * 1024;

export type PeekFields = {
  magic?: string;
  network?: string;
  unifiedAddress?: string;
  transparentAddress?: string | null;
  birthday?: number;
  scannedHeight?: number;
  treesReady?: boolean;
  viewOnly?: boolean;
  hardware?: WalletSnapshot["hardware"] | null;
  ufvk?: string;
  orchardAvailable?: number;
  totalAvailable?: number;
  totalPending?: number;
  notes?: Array<{ pool?: string; valueZat?: number; spent?: boolean }>;
  utxos?: Array<{ valueZat?: number; spent?: boolean }>;
};

export function jsonStringField(text: string, key: string): string | undefined {
  const m = new RegExp(`"${key}"\\s*:\\s*"((?:\\\\.|[^"\\\\])*)"`).exec(text);
  return m?.[1];
}

export function jsonNumField(text: string, key: string): number | undefined {
  const m = new RegExp(`"${key}"\\s*:\\s*(-?\\d+)`).exec(text);
  return m ? Number(m[1]) : undefined;
}

export function jsonHardwareField(text: string): WalletSnapshot["hardware"] {
  const m = /"hardware"\s*:\s*(\{[^{}]*\})/.exec(text);
  if (!m) return undefined;
  try {
    const h = JSON.parse(m[1]!) as WalletSnapshot["hardware"];
    return h && (h.device === "keystone" || h.device === "ledger") ? h : undefined;
  } catch {
    return undefined;
  }
}

export function jsonBoolField(text: string, key: string): boolean | undefined {
  const m = new RegExp(`"${key}"\\s*:\\s*(true|false)`).exec(text);
  if (!m) return undefined;
  return m[1] === "true";
}

export function extractJsonArray(text: string, start: number): string | null {
  if (text[start] !== "[") return null;
  let depth = 0;
  let inStr = false;
  let esc = false;
  for (let i = start; i < text.length; i++) {
    const c = text[i];
    if (inStr) {
      if (esc) esc = false;
      else if (c === "\\") esc = true;
      else if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') inStr = true;
    else if (c === "[") depth += 1;
    else if (c === "]") {
      depth -= 1;
      if (depth === 0) return text.slice(start, i + 1);
    }
  }
  return null;
}

export function jsonNamedArray(text: string, key: string): unknown[] | undefined {
  const m = new RegExp(`"${key}"\\s*:\\s*\\[`).exec(text);
  if (!m || m.index == null) return undefined;
  const start = text.indexOf("[", m.index);
  if (start < 0) return undefined;
  const slice = extractJsonArray(text, start);
  if (!slice) return undefined;
  try {
    const parsed: unknown = JSON.parse(slice);
    return Array.isArray(parsed) ? parsed : undefined;
  } catch {
    return undefined;
  }
}

export function walletFromPeek(snap: PeekFields): WalletSnapshot | null {
  if (snap.magic !== "zstk1" || !snap.unifiedAddress) return null;
  let sapling = 0;
  let orchard = 0;
  let ironwood = 0;
  for (const n of snap.notes ?? []) {
    if (n.spent) continue;
    const v = Number(n.valueZat) || 0;
    if (n.pool === "sapling") sapling += v;
    else if (n.pool === "ironwood") ironwood += v;
    else orchard += v;
  }
  let transparent = 0;
  for (const u of snap.utxos ?? []) {
    if (!u.spent) transparent += Number(u.valueZat) || 0;
  }
  if (!(snap.notes && snap.notes.length) && snap.orchardAvailable) {
    orchard = Number(snap.orchardAvailable) || 0;
  }
  const headerAvailable = Number(snap.totalAvailable) || 0;
  const total = sapling + orchard + ironwood + transparent || headerAvailable;
  const pending = Number(snap.totalPending) || 0;
  const ua = snap.unifiedAddress;
  return {
    network: snap.network || "",
    server: "",
    birthdayHeight: snap.birthday ?? 1,
    unifiedAddress: ua,
    transparentAddress: snap.transparentAddress ?? null,
    zip321: zip321Uri(ua),
    scannedHeight: snap.scannedHeight,
    treesReady: !!snap.treesReady,
    spendReady: false,
    // A hardware wallet spends through its device.
    viewOnly: !!snap.viewOnly && !snap.hardware,
    ...(snap.hardware ? { hardware: snap.hardware } : {}),
    ufvk: snap.ufvk,
    balance: {
      saplingAvailable: sapling,
      orchardAvailable: orchard,
      ironwoodAvailable: ironwood,
      transparentAvailable: transparent,
      totalAvailable: total,
      totalPending: pending,
      saplingZec: formatZatoshis(BigInt(sapling)),
      orchardZec: formatZatoshis(BigInt(orchard)),
      ironwoodZec: formatZatoshis(BigInt(ironwood)),
      transparentZec: formatZatoshis(BigInt(transparent)),
      totalZec: formatZatoshis(BigInt(total)),
      pendingZec: formatZatoshis(BigInt(pending)),
    },
  };
}

/** UA + keys + notes from the start of a zstk1 blob. Works on truncated JSON (full parse not required). */
export function peekSnapshotHeader(bytes: Uint8Array): WalletSnapshot | null {
  const text = new TextDecoder().decode(bytes.subarray(0, Math.min(bytes.byteLength, 512 * 1024)));
  if (!/"magic"\s*:\s*"zstk1"/.test(text)) return null;
  const unifiedAddress = jsonStringField(text, "unifiedAddress");
  if (!unifiedAddress) return null;
  const notes = jsonNamedArray(text, "notes") as PeekFields["notes"];
  const utxos = jsonNamedArray(text, "utxos") as PeekFields["utxos"];
  return walletFromPeek({
    magic: "zstk1",
    network: jsonStringField(text, "network"),
    unifiedAddress,
    transparentAddress: jsonStringField(text, "transparentAddress") ?? null,
    birthday: jsonNumField(text, "birthday"),
    scannedHeight: jsonNumField(text, "scannedHeight"),
    treesReady: jsonBoolField(text, "treesReady"),
    viewOnly: jsonBoolField(text, "viewOnly"),
    hardware: jsonHardwareField(text),
    ufvk: jsonStringField(text, "ufvk"),
    orchardAvailable: jsonNumField(text, "orchardAvailable"),
    totalAvailable: jsonNumField(text, "totalAvailable"),
    totalPending: jsonNumField(text, "totalPending"),
    notes,
    utxos,
  });
}

export function peekSnapshotJson(bytes: Uint8Array): WalletSnapshot | null {
  try {
    return walletFromPeek(JSON.parse(new TextDecoder().decode(bytes)) as PeekFields);
  } catch {
    return null;
  }
}

/** Last-known UA + balances from IndexedDB JSON. No wasm, no trees, no network. */
export function peekSnapshotBytes(bytes: Uint8Array): WalletSnapshot | null {
  if (bytes.byteLength > PEEK_FULL_PARSE_MAX) {
    return peekSnapshotHeader(bytes) ?? peekSnapshotJson(bytes);
  }
  return peekSnapshotJson(bytes) ?? peekSnapshotHeader(bytes);
}
