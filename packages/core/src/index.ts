/**
 * @module @z-stack/core
 *
 * @z-stack/core — platform-free helpers shared by web and (later) mobile.
 * No DOM / React / React Native types allowed here.
 *
 * Keep SyncStage in lockstep with crates/z-engine/src/lib.rs (serde rename_all = snake_case).
 * Prefer generating this union from Rust later — do not drift by hand.
 */

export type SyncStage =
  | "idle"
  | "connecting"
  | "downloading"
  | "scanning"
  | "enhancing"
  | "catching_up"
  | "synced"
  | "error";

export function isWalletDataFinalStage(stage: SyncStage): boolean {
  return stage === "synced" || stage === "enhancing" || stage === "catching_up";
}

/** 1 ZEC = 100_000_000 zatoshis. Keep lockstep with `z_engine::ZATOSHI_PER_ZEC`. */
export const ZATOSHI_PER_ZEC = 100_000_000n;

/** Default shield threshold used by `z-wallet serve` / NativeWallet. */
export const SHIELD_THRESHOLD_ZAT = 100_000n;

/** Conservative ZIP-317 pad so UI can gate Send before the engine proposes. */
export const FEE_PAD_ZAT = 10_000n;

/** Format zatoshis as ZEC with half-up rounding to `fractionDigits` (max 8). */
export function formatZatoshis(zats: bigint, fractionDigits = 8): string {
  const digits = Math.min(8, Math.max(0, fractionDigits | 0));
  const neg = zats < 0n;
  const v = neg ? -zats : zats;
  const whole = v / 100_000_000n;
  const fracFull = v % 100_000_000n;
  if (digits === 0) {
    // round half up at 0.5 ZEC
    const rounded = whole + (fracFull >= 50_000_000n ? 1n : 0n);
    return `${neg ? "-" : ""}${rounded}`;
  }
  const scale = 10n ** BigInt(8 - digits);
  const roundedFrac = (fracFull + scale / 2n) / scale;
  const carry = roundedFrac >= 10n ** BigInt(digits) ? 1n : 0n;
  const frac = (roundedFrac % 10n ** BigInt(digits)).toString().padStart(digits, "0");
  const body = `${whole + carry}.${frac}`;
  return neg ? `-${body}` : body;
}

export type HistoryStatus = "mined" | "pending" | "expired";

/**
 * One `v_transactions` row, camelCased. Native sqlite and WASM snapshot
 * return this same shape (`z_engine::HistoryEntry`).
 *
 * Zat fields are JSON `number` (Rust `i64`/`u64`), not `bigint`, so the
 * loopback bridge and IndexedDB snapshot stay one type. Classify at read
 * time with {@link classifyHistory} — never persist `type` / `displayValue`.
 *
 * | Field | `v_transactions` |
 * |-------|------------------|
 * | `txid` | `txid` |
 * | `minedHeight` | `mined_height` |
 * | `expiryHeight` | `expiry_height` |
 * | `accountDeltaZat` | `account_balance_delta` |
 * | `spentZat` | `total_spent` |
 * | `receivedZat` | `total_received` |
 * | `feeZat` | `fee_paid` |
 * | `sentNoteCount` / `receivedNoteCount` / `memoCount` | same names |
 * | `hasChange` | `has_change` |
 * | `isShielding` | `is_shielding` |
 * | `expiredUnmined` | `expired_unmined` |
 *
 * Per-pool columns are classifier input; native view leaves them `0`.
 */
export type HistoryEntry = {
  txid: string;
  status: HistoryStatus;
  minedHeight: number | null;
  expiryHeight: number | null;
  accountDeltaZat: number;
  spentZat: number;
  receivedZat: number;
  feeZat: number | null;
  sentNoteCount: number;
  receivedNoteCount: number;
  memoCount: number;
  hasChange: boolean;
  isShielding: boolean;
  expiredUnmined: boolean;
  memos?: string[];
  /** Unknown for legacy snapshots/providers; empty means no text memo was recovered. */
  memoStatus?: "pending" | "available" | "empty" | "unavailable" | "unknown" | "notApplicable";
  blockTime?: number | null;
  confirmations?: number | null;
  /** Per-pool movement (zat). Classifier input; 0 when unknown (native sqlite view). */
  transparentReceived?: number;
  transparentSpent?: number;
  saplingReceived?: number;
  saplingSpent?: number;
  orchardReceived?: number;
  orchardSpent?: number;
  ironwoodReceived?: number;
  ironwoodSpent?: number;
  historyMetadataComplete?: boolean;
  outgoingShieldedZat?: number;
  transparentInputs?: { txid: string; index: number }[];
  transparentOutputs?: { index: number; valueZat: number }[];
};

export type WalletSnapshot = {
  network: string;
  server: string;
  /** Local Zakura/Zebra JSON-RPC, independent of `server` (Zaino). */
  validatorRpc?: string | null;
  birthdayHeight: number;
  unifiedAddress: string;
  transparentAddress: string | null;
  zip321: string;
  scannedHeight?: number;
  transparentScanHeight?: number | null;
  transparentScanComplete?: boolean;
  memoScanHeight?: number | null;
  transparentScanStatus?: "off" | "unsupported" | "scanning" | "complete" | "unavailable";
  /** Memo retrieval progress. On-demand starts off; after fetchMemos(), scanning means another batch remains. */
  memoFetchStatus?: "off" | "unsupported" | "scanning" | "complete" | "unavailable";
  sharedMemoStatus?: "off" | "unsupported" | "scanning" | "complete" | "unavailable";
  /** Birthday frontier / subtree roots are installed (witnesses can be built). */
  treesReady?: boolean;
  /** treesReady and at least one unspent orchard/ironwood note (send can be proposed). */
  spendReady?: boolean;
  balance: {
    saplingAvailable: number;
    orchardAvailable: number;
    ironwoodAvailable: number;
    transparentAvailable: number;
    totalAvailable: number;
    saplingPending?: number;
    orchardPending?: number;
    ironwoodPending?: number;
    transparentPending?: number;
    totalPending?: number;
    saplingZec: string;
    orchardZec: string;
    ironwoodZec: string;
    transparentZec: string;
    totalZec: string;
    pendingZec?: string;
    orchardPendingZec?: string;
    transparentPendingZec?: string;
  };
  confirmations?: {
    trusted: number;
    untrusted: number;
    zeroConfShield: boolean;
  };
  txids?: string[];
  mnemonic?: string;
  /** No seed and no hardware wallet: this device can watch but not spend. */
  viewOnly?: boolean;
  /** Set for a hardware-wallet account: sends are signed on this device. */
  hardware?: { device: "keystone" | "ledger"; seedFingerprint: string; accountIndex: number };
  maxSendZat?: number;
  maxSendZec?: string;
  ufvk?: string;
  recentRecipients?: string[];
  unlockPolicy?: UnlockPolicy;
  /** Recent history (same `HistoryEntry` rows as `EngineClient.history`). */
  transactions?: HistoryEntry[];
};

/** When the spending seed is requested. Keep lockstep with `z_engine::native::UnlockPolicy`. */
export type UnlockPolicy = "session" | "each-spend" | "always";

/**
 * ZIP-321 payment URI (single and `address.N` multi-pay).
 * Keep lockstep with `z_engine::zip321_uri` / `parse_zip321`.
 * Amount is decimal ZEC (max 8 fractional digits), not zatoshis.
 */
function b64urlEncode(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  const b64 = btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  return b64;
}

function b64urlDecode(s: string): string {
  const pad = s.length % 4 === 0 ? "" : "=".repeat(4 - (s.length % 4));
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/") + pad;
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder().decode(bytes);
}

export type Zip321Extras = { memo?: string; label?: string; message?: string };

export type Zip321Payment = {
  address: string;
  amountZec?: string;
  amountZat?: bigint;
  memo?: string;
  label?: string;
  message?: string;
};

export type Zip321Request = Zip321Payment & { payments: Zip321Payment[] };

function assertUriSafeAddress(addr: string): string {
  const a = addr.trim();
  if (!a) {
    throw new Error("empty address");
  }
  if (/[\s?&#]/.test(a)) {
    throw new Error("address is not URI-safe");
  }
  return a;
}

function assertZecAmount(amount: string): void {
  if (!/^(?:0|[1-9]\d*)?(?:\.\d{1,8})?$/.test(amount) || amount === ".") {
    throw new Error("amount must be a non-negative decimal ZEC value");
  }
}

function zip321ParamKey(k: string): { idx: number; name: string } {
  const dot = k.indexOf(".");
  if (dot >= 0) {
    const rest = k.slice(dot + 1);
    if (/^\d+$/.test(rest)) return { idx: Number(rest), name: k.slice(0, dot) };
  }
  return { idx: 0, name: k };
}

export function zip321Uri(address: string, amountZec?: string, extras?: Zip321Extras): string {
  const addr = assertUriSafeAddress(address);
  const q: string[] = [];
  const amount = amountZec?.trim();
  if (amount) {
    assertZecAmount(amount);
    q.push(`amount=${amount}`);
  }
  const memo = extras?.memo?.trim();
  if (memo) q.push(`memo=${b64urlEncode(memo)}`);
  const label = extras?.label?.trim();
  if (label) q.push(`label=${encodeURIComponent(label)}`);
  const message = extras?.message?.trim();
  if (message) q.push(`message=${encodeURIComponent(message)}`);
  return q.length ? `zcash:${addr}?${q.join("&")}` : `zcash:${addr}`;
}

/** `zcash:addr0?amount=1&address.1=addr1&amount.1=2&memo.1=...` */
export function zip321UriMany(payments: Zip321Payment[]): string {
  if (!payments.length) {
    throw new Error("ZIP-321 needs at least one payment");
  }
  let uri = zip321Uri(payments[0].address, payments[0].amountZec, {
    memo: payments[0].memo,
    label: payments[0].label,
    message: payments[0].message,
  });
  for (let i = 1; i < payments.length; i++) {
    const addr = assertUriSafeAddress(payments[i].address);
    uri += `${uri.includes("?") ? "&" : "?"}address.${i}=${addr}`;
    const amount = payments[i].amountZec?.trim();
    if (amount) {
      assertZecAmount(amount);
      uri += `&amount.${i}=${amount}`;
    }
    const memo = payments[i].memo?.trim();
    if (memo) uri += `&memo.${i}=${b64urlEncode(memo)}`;
    const label = payments[i].label?.trim();
    if (label) uri += `&label.${i}=${encodeURIComponent(label)}`;
    const message = payments[i].message?.trim();
    if (message) uri += `&message.${i}=${encodeURIComponent(message)}`;
  }
  return uri;
}

/** Parse a decimal ZEC amount into zatoshis. Keep lockstep with `z_engine::parse_zec_to_zatoshis`. */
export function parseZecToZatoshis(s: string): bigint {
  const t = s.trim();
  if (!t) throw new Error("empty amount");
  if (t.includes("-") || t.includes("+") || t.includes("e") || t.includes("E")) {
    throw new Error("amount must be a non-negative decimal ZEC value");
  }
  const [whole, frac = ""] = t.includes(".") ? t.split(".", 2) : [t, ""];
  if (!whole && !frac) throw new Error("empty amount");
  if (
    ![...whole].every((c) => c >= "0" && c <= "9") ||
    ![...frac].every((c) => c >= "0" && c <= "9")
  ) {
    throw new Error("amount must be a non-negative decimal ZEC value");
  }
  if (frac.length > 8) throw new Error("more than 8 decimal places");
  const wholeN = whole ? BigInt(whole) : 0n;
  const fracN = BigInt(frac.padEnd(8, "0") || "0");
  return wholeN * ZATOSHI_PER_ZEC + fracN;
}

function paymentFromSlot(slot: Zip321Payment): Zip321Payment {
  const amountZat = slot.amountZec ? parseZecToZatoshis(slot.amountZec) : undefined;
  return { ...slot, amountZat };
}

/** Old SDK name for {@link parseZecToZatoshis}. Safe (bigint); do not add a `number` converter. */
export const zecToZatoshis = parseZecToZatoshis;

/** Inverse of `zip321Uri` / `zip321UriMany`. Keep lockstep with `z_engine::parse_zip321`. */
export function parseZip321(uri: string): Zip321Request {
  const raw = uri.trim();
  if (!raw.toLowerCase().startsWith("zcash:")) {
    throw new Error("not a zcash: URI");
  }
  const rest = raw.slice("zcash:".length);
  const q = rest.indexOf("?");
  const pathAddr = (q < 0 ? rest : rest.slice(0, q)).trim();
  if (pathAddr && /[\s?&#]/.test(pathAddr)) {
    throw new Error("address is not URI-safe");
  }
  const byIndex = new Map<number, Zip321Payment>();
  if (pathAddr) byIndex.set(0, { address: pathAddr });
  if (q >= 0) {
    for (const part of rest.slice(q + 1).split("&")) {
      if (!part) continue;
      const eq = part.indexOf("=");
      const k = decodeURIComponent(eq < 0 ? part : part.slice(0, eq));
      const v = eq < 0 ? "" : decodeURIComponent(part.slice(eq + 1));
      const { idx, name: rawName } = zip321ParamKey(k);
      const name = rawName.startsWith("req-") ? rawName.slice(4) : rawName;
      if (
        rawName.startsWith("req-") &&
        !["address", "amount", "memo", "label", "message"].includes(name)
      ) {
        throw new Error(`unsupported required ZIP-321 parameter: ${rawName}`);
      }
      const slot = byIndex.get(idx) ?? { address: "" };
      if (name === "address" && v) slot.address = v;
      if (name === "amount" && v) {
        parseZecToZatoshis(v);
        slot.amountZec = v;
      }
      if (name === "memo" && v) {
        try {
          slot.memo = b64urlDecode(v);
        } catch {
          slot.memo = v;
        }
      }
      if (name === "label" && v) slot.label = v;
      if (name === "message" && v) slot.message = v;
      byIndex.set(idx, slot);
    }
  }
  const first = byIndex.get(0);
  if (!first?.address?.trim()) {
    throw new Error("ZIP-321 URI has no address");
  }
  const max = Math.max(...byIndex.keys());
  const payments: Zip321Payment[] = [];
  for (let i = 0; i <= max; i++) {
    const slot = byIndex.get(i);
    if (!slot?.address?.trim()) {
      throw new Error("ZIP-321 payment indices must be sequential");
    }
    payments.push(paymentFromSlot(slot));
  }
  const primary = payments[0];
  return { ...primary, payments };
}

export function shieldedAvailableZat(b: {
  saplingAvailable: number;
  orchardAvailable: number;
  ironwoodAvailable: number;
}): bigint {
  return BigInt(b.saplingAvailable) + BigInt(b.orchardAvailable) + BigInt(b.ironwoodAvailable);
}

export function canShield(
  transparentAvailable: number,
  thresholdZat = SHIELD_THRESHOLD_ZAT,
): boolean {
  return BigInt(transparentAvailable) >= thresholdZat;
}

export type SendBlockCode =
  | "ok"
  | "invalid_amount"
  | "view_only"
  | "not_spend_ready"
  | "no_seed"
  | "insufficient_funds";

export type SendCheck =
  | { ok: true; code: "ok" }
  | { ok: false; code: Exclude<SendBlockCode, "ok">; message: string };

const SEND_BLOCK_MESSAGE: Record<Exclude<SendBlockCode, "ok">, string> = {
  invalid_amount: "Enter an amount greater than zero",
  view_only: "This wallet is view-only",
  not_spend_ready: "Still building spend witnesses",
  no_seed: "Re-enter the spending seed to send",
  insufficient_funds: "Not enough shielded balance for this amount and fee",
};

/**
 * Why a send can or cannot be proposed.
 * `viewOnly` / `spendReady` / `hasSeed` are checked only when passed.
 * Omitted flags keep the balance-only check {@link canSend} has always done.
 */
export function canSendReason(opts: {
  balance: { saplingAvailable: number; orchardAvailable: number; ironwoodAvailable: number };
  amountZat: bigint;
  feePadZat?: bigint;
  viewOnly?: boolean;
  spendReady?: boolean;
  hasSeed?: boolean;
}): SendCheck {
  const fee = opts.feePadZat ?? FEE_PAD_ZAT;
  const block = (code: Exclude<SendBlockCode, "ok">): SendCheck => ({
    ok: false,
    code,
    message: SEND_BLOCK_MESSAGE[code],
  });
  if (opts.amountZat <= 0n) return block("invalid_amount");
  if (opts.viewOnly) return block("view_only");
  if (opts.hasSeed === false) return block("no_seed");
  if (opts.spendReady === false) return block("not_spend_ready");
  if (shieldedAvailableZat(opts.balance) < opts.amountZat + fee) return block("insufficient_funds");
  return { ok: true, code: "ok" };
}

export function canSend(
  balance: { saplingAvailable: number; orchardAvailable: number; ironwoodAvailable: number },
  amountZat: bigint,
  feePadZat = FEE_PAD_ZAT,
): boolean {
  return canSendReason({ balance, amountZat, feePadZat }).ok;
}

export {
  BATCH_GRPC_WEB,
  BATCH_LWD_PIPE,
  BATCH_LOCAL,
  BATCH_PUBLIC,
  BLOCK_SECONDS,
  DATE_SAFETY_BLOCKS,
  HASH_KEEP,
  MAX_SYNC_BLOCKS,
  NEAR_TIP_BLOCKS,
  PREFETCH_GRPC_WEB,
  PREFETCH_GRPC_WEB_MAX,
  PREFETCH_LWD_PIPE,
  PERSIST_EVERY_LWD_PIPE,
  QUIET_BEHIND_BLOCKS,
  STALL_QUIET_REMAINING,
  SYNC_STALL_MS,
  catchUpPercent,
  dateFromHeight,
  displayCatchUpPercent,
  historicOverlayVisible,
  fmtSecs,
  grpcWebPrefetch,
  pipePrefetch,
  prefetchBuffer,
  heightFromDate,
  lightStallWarning,
  liveScanEtaSecs,
  blockSpacingSeconds,
  type BirthdayNetwork,
  parseBirthdayInput,
  validateBirthdayInput,
  scanRateRange,
  syncEta,
  syncTuning,
  typicalTip,
  ymdDaysAgo,
  type SyncTuning,
  type SyncTuningOpts,
} from "./birthday";

export {
  classifyHistory,
  classifyHistoryList,
  filterHistory,
  historyStatusOf,
  type HistoryQuery,
  computeDisplayValue,
  formatHistoryTime,
  historyActionLabel,
  inferTransactionType,
  type ClassifiedHistory,
  type TxAction,
} from "./history";

export {
  WALLET_ERROR_MESSAGES,
  WalletError,
  classifyWalletError,
  isWalletError,
  walletErrorMessage,
  type WalletErrorCode,
} from "./errors";

export {
  DEFAULT_UA_RECEIVER_SET,
  UA_RECEIVERS,
  classifyUaReceiverSet,
  inspectAddressSummary,
  isUaReceiverSet,
  parseUaReceiverSet,
  uaIncludesTransparent,
  uaReceiverSetLabel,
  uaReceivers,
  type InspectedAddress,
  type UaReceiver,
  type UaReceiverSet,
} from "./ua";
