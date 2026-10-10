/**
 * Birthday height from a civil date (YYYY-MM-DD) or the reverse.
 * Keep lockstep with `z_engine::birthday`.
 */

import { WalletError } from "./errors";

/** Post-Blossom, pre-NU7 spacing; use blockSpacingSeconds for a network and height. */
export const BLOCK_SECONDS = 75;
export const DATE_SAFETY_BLOCKS = 200;

export function typicalTip(network: string): number {
  switch (network) {
    case "mainnet":
      return 3_470_000;
    case "testnet":
      return 4_470_000;
    default:
      return 200;
  }
}

/** Keep lockstep with `z_engine::scan`. */
export const BATCH_LOCAL = 4_000;
export const BATCH_PUBLIC = 1_000;
/** gRPC-Web is unary HTTP: the whole range is buffered before JS sees a byte. */
export const BATCH_GRPC_WEB = 1_000;
/** Local pipe only: measured faster per block at 2k, while still below the 8k HTTP cap. */
export const BATCH_LWD_PIPE = 2_000;
export const PREFETCH_LOCAL = 4;
export const PREFETCH_PUBLIC = 2;
/** Unresolved GetBlockRange count (not buffered-but-unapplied blobs). Fetch parallelism is independent of Rayon; 8 matches default wasm threads. Chrome HTTP/1.1 may only use 6 sockets to one host. */
export const PREFETCH_GRPC_WEB = 8;
export const PREFETCH_GRPC_WEB_MAX = 16;
/** 4×2k keeps the same 8k in-flight and 16k reserved heights as 8×1k. */
export const PREFETCH_LWD_PIPE = 4;
export const PERSIST_EVERY_LOCAL = 16;
export const PERSIST_EVERY_PUBLIC = 8;
export const PERSIST_EVERY_GRPC_WEB = 32;
/** Keep the pipe's crash/restart loss window near 16k heights at 2k per batch. */
export const PERSIST_EVERY_LWD_PIPE = 8;
export const MAX_SYNC_BLOCKS = 150_000;
/** Session catch-up this small is “almost at tip”, not a full-chain sync. Keep lockstep with `z_engine::birthday`. */
export const NEAR_TIP_BLOCKS = 512;
/** Compact-block fetch / scan has not advanced for this long → consider a stall. */
export const SYNC_STALL_MS = 15_000;
/** Remaining this small is catch-up, not a dead light URL. Keep lockstep with `z_engine::birthday`. */
export const STALL_QUIET_REMAINING = 32;
/** Last few blocks (verify window / 3-conf + slack). Keep lockstep with `z_engine::birthday`. */
export const QUIET_BEHIND_BLOCKS = 10;
/** WASM reorg hash window. Keep lockstep with `z_engine::scan::HASH_KEEP`. */
export const HASH_KEEP = 2_048;

/**
 * Whether to surface “check the light URL” stall copy.
 * Near tip (span ≤ {@link NEAR_TIP_BLOCKS} or remaining ≤ {@link STALL_QUIET_REMAINING})
 * stays Catching up even if heights freeze. Far from tip: only if frozen and the light probe failed.
 */
export function lightStallWarning(
  span: number,
  remaining: number,
  frozen: boolean,
  lightReachable: boolean,
): boolean {
  return (
    frozen && remaining > STALL_QUIET_REMAINING && span > NEAR_TIP_BLOCKS && !lightReachable
  );
}

/**
 * Catch-up bar from *this session’s* origin→tip (not birthday→tip).
 * Near tip floors at 90% so ~54 blocks behind never looks like 4%.
 * Far from tip (birthday restore or a 108k island→tip gap): 0% until
 * download or scan leaves origin. Do not floor a huge gap at 90%.
 */
export function catchUpPercent(
  origin: number,
  scanned: number,
  downloaded: number,
  tip: number,
  _birthday = origin,
): number {
  const span = Math.max(1, tip - origin);
  const sc = Math.max(0, Math.min(span, Math.max(origin, scanned) - origin));
  const dl = Math.max(0, Math.min(span, Math.max(origin, downloaded, scanned) - origin));
  const pct =
    span <= NEAR_TIP_BLOCKS
      ? 90 + (9 * sc) / span
      : dl === 0 && sc === 0
        ? 0
        : 36 * (dl / span) + 50 * (sc / span);
  return Math.min(99, pct);
}

/** Full historic overlay vs compact “N behind”. Hide only when remaining ≤ 32. */
export function historicOverlayVisible(remaining: number | null | undefined): boolean {
  if (remaining == null) return true;
  if (remaining <= 0) return false;
  return remaining > STALL_QUIET_REMAINING;
}

/** UI guard when the engine/client still reports a connecting floor. */
export function displayCatchUpPercent(percent: number, scanned: number, tip: number): number {
  const left = Math.max(0, tip - scanned);
  if (tip > 0 && left > 0 && left <= NEAR_TIP_BLOCKS) {
    return Math.min(99, Math.max(percent, 90));
  }
  return Math.max(0, Math.min(100, percent));
}

export type SyncTuning = { batch: number; prefetch: number; persistEvery: number };
export type SyncTuningOpts = { grpcWeb?: boolean; lwdPipe?: boolean };

export function syncTuning(localLight: boolean, opts?: SyncTuningOpts): SyncTuning {
  if (opts?.grpcWeb) {
    return {
      batch: BATCH_GRPC_WEB,
      prefetch: PREFETCH_GRPC_WEB,
      persistEvery: PERSIST_EVERY_GRPC_WEB,
    };
  }
  if (opts?.lwdPipe) {
    return {
      batch: BATCH_LWD_PIPE,
      prefetch: PREFETCH_LWD_PIPE,
      persistEvery: PERSIST_EVERY_LWD_PIPE,
    };
  }
  return localLight
    ? { batch: BATCH_LOCAL, prefetch: PREFETCH_LOCAL, persistEvery: PERSIST_EVERY_LOCAL }
    : { batch: BATCH_PUBLIC, prefetch: PREFETCH_PUBLIC, persistEvery: PERSIST_EVERY_PUBLIC };
}

/**
 * Parallel GetBlockRange for gRPC-Web: at least 8, up to threads, hard cap 16.
 * Constrained radio (save-data / 2G–3G) stays at 4 so Safari/cellular does not pin sockets.
 */
export function grpcWebPrefetch(threads?: number, constrained = false): number {
  if (constrained) return Math.min(4, PREFETCH_GRPC_WEB);
  const t = threads && threads > 0 ? Math.floor(threads) : PREFETCH_GRPC_WEB;
  return Math.min(PREFETCH_GRPC_WEB_MAX, Math.max(PREFETCH_GRPC_WEB, t));
}

/** Parallel `/lwd/blocks` on the Zaino pipe. Do not scale with wasm threads (that hits 16 and pins Zaino). */
export function pipePrefetch(constrained = false): number {
  return constrained ? Math.min(2, PREFETCH_LWD_PIPE) : PREFETCH_LWD_PIPE;
}

/** Downloaded-but-not-applied cap. Must be 2× prefetch so apply lag cannot stall the next HTTP start. */
export function prefetchBuffer(prefetch: number): number {
  return Math.max(1, Math.floor(prefetch)) * 2;
}

/** Rough compact-block scan rates (blocks / second) for ETA copy. Local Zaino is the 90-day / ~1 min path. */
export function scanRateRange(localLight: boolean, opts?: SyncTuningOpts): { lo: number; hi: number } {
  if (opts?.grpcWeb) return { lo: 80, hi: 400 };
  if (opts?.lwdPipe) return { lo: 80, hi: 400 };
  return localLight ? { lo: 400, hi: 1_800 } : { lo: 20, hi: 80 };
}

export function fmtSecs(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 90) return `${s}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  const h = Math.floor(s / 3600);
  const m = Math.round((s % 3600) / 60);
  return m === 0 ? `${h}h` : `${h}h ${m}m`;
}

export function syncEta(
  birthday: number,
  tip: number,
  localLight: boolean,
  opts?: SyncTuningOpts,
): { blocks: number; secondsFast: number; secondsSlow: number; human: string } {
  const blocks = Math.max(1, tip - birthday);
  const { lo, hi } = scanRateRange(localLight, opts);
  const secondsFast = Math.max(1, Math.floor(blocks / hi));
  const secondsSlow = Math.max(1, Math.floor(blocks / lo));
  return {
    blocks,
    secondsFast,
    secondsSlow,
    human: `${blocks} blocks · about ${fmtSecs(secondsFast)}–${fmtSecs(secondsSlow)}`,
  };
}

/** Live overlay clock. None until scan moved (`scDone > 1`). Gap fill / far-from-tip stay on “N behind”. */
export function liveScanEtaSecs(
  scDone: number,
  remaining: number,
  origin: number,
  birthday: number,
  scanBps: number,
): number | undefined {
  if (scDone <= 1 || remaining <= 0 || scanBps <= 1) return undefined;
  if (origin > birthday || remaining > NEAR_TIP_BLOCKS) return undefined;
  return Math.max(1, Math.round(remaining / scanBps));
}

function nowUnix(): number {
  return Math.floor(Date.now() / 1000);
}

function isLeap(y: number): boolean {
  return y % 4 === 0 && (y % 100 !== 0 || y % 400 === 0);
}

function daysInMonth(y: number, m: number): number {
  switch (m) {
    case 1:
    case 3:
    case 5:
    case 7:
    case 8:
    case 10:
    case 12:
      return 31;
    case 4:
    case 6:
    case 9:
    case 11:
      return 30;
    case 2:
      return isLeap(y) ? 29 : 28;
    default:
      return 0;
  }
}

/** Howard Hinnant days_from_civil (Unix epoch). */
function daysFromCivil(y: number, m: number, d: number): number {
  y -= m <= 2 ? 1 : 0;
  const era = Math.trunc((y >= 0 ? y : y - 399) / 400);
  const yoe = y - era * 400;
  const mp = m > 2 ? m - 3 : m + 9;
  const doy = Math.trunc((153 * mp + 2) / 5) + d - 1;
  const doe = yoe * 365 + Math.trunc(yoe / 4) - Math.trunc(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}

function civilFromDays(z: number): { y: number; m: number; d: number } {
  z += 719468;
  const era = Math.trunc((z >= 0 ? z : z - 146096) / 146097);
  const doe = z - era * 146097;
  const yoe = Math.trunc((doe - Math.trunc(doe / 1460) + Math.trunc(doe / 36524) - Math.trunc(doe / 146096)) / 365);
  let y = yoe + era * 400;
  const doy = doe - (365 * yoe + Math.trunc(yoe / 4) - Math.trunc(yoe / 100));
  const mp = Math.trunc((5 * doy + 2) / 153);
  const d = doy - Math.trunc((153 * mp + 2) / 5) + 1;
  const m = mp < 10 ? mp + 3 : mp - 9;
  y += m <= 2 ? 1 : 0;
  return { y, m, d };
}

function ymdToUnix(s: string): number {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(s.trim())) throw new WalletError("invalid_birthday", "invalid birthday date");
  const parts = s.trim().split("-");
  if (parts.length !== 3) throw new WalletError("invalid_birthday", "invalid birthday date");
  const y = Number(parts[0]);
  const m = Number(parts[1]);
  const d = Number(parts[2]);
  if (!Number.isInteger(y) || !Number.isInteger(m) || !Number.isInteger(d)) {
    throw new WalletError("invalid_birthday", "invalid birthday date");
  }
  if (m < 1 || m > 12 || d < 1 || d > daysInMonth(y, m)) {
    throw new WalletError("invalid_birthday", "invalid birthday date");
  }
  return daysFromCivil(y, m, d) * 86400;
}

function unixToYmd(unix: number): string {
  const { y, m, d } = civilFromDays(Math.floor(unix / 86400));
  return `${String(y).padStart(4, "0")}-${String(m).padStart(2, "0")}-${String(d).padStart(2, "0")}`;
}

export type BirthdayNetwork = string | {
  network: string;
  /** Regtest only; match the validator. NU7 is otherwise unscheduled. */
  regtestNu7Height?: number;
};

function spacingEras(config: BirthdayNetwork): Array<[number, number]> {
  const network = typeof config === "string" ? config : config.network;
  if (!["mainnet", "testnet", "regtest"].includes(network)) throw new WalletError("invalid_birthday", "invalid network");
  const eras: Array<[number, number]> = [[1, 150]];
  eras.push([network === "mainnet" ? 653_600 : network === "testnet" ? 584_000 : 1, 75]);
  // Common 2.2.0 does not schedule mainnet NU7. Never infer it from wall time.
  const nu7 = network === "testnet" ? 4_465_026 : network === "regtest" && typeof config !== "string" ? config.regtestNu7Height : undefined;
  if (nu7 !== undefined) {
    if (!Number.isInteger(nu7) || nu7 < 3 || nu7 > 0xffff_ffff) {
      throw new WalletError("invalid_birthday", "invalid regtest NU7 height");
    }
    eras.push([nu7, 25]);
  }
  return eras;
}

/** Consensus spacing at a height, in seconds. */
export function blockSpacingSeconds(network: BirthdayNetwork, height: number): number {
  return spacingEras(network).filter(([start]) => height >= start).at(-1)?.[1] ?? 150;
}

function heightBeforeSeconds(tip: number, seconds: number, network: BirthdayNetwork): number {
  let height = Math.max(1, tip);
  for (const [start, spacing] of spacingEras(network).reverse()) {
    if (height < start) continue;
    const span = (height - start + 1) * spacing;
    if (seconds < span) return Math.max(1, height - Math.floor(seconds / spacing));
    seconds -= span;
    height = start - 1;
  }
  return 1;
}

/** Approximate conversion from a live tip and wall clock, across spacing changes.
 * Prefer the wallet's exact birthday height when available.
 */
export function heightFromDate(ymd: string, tipHeight: number, network: BirthdayNetwork = "mainnet"): number {
  const seconds = Math.max(0, nowUnix() - ymdToUnix(ymd));
  return heightBeforeSeconds(tipHeight, seconds, network);
}

export function dateFromHeight(height: number, tipHeight: number, network: BirthdayNetwork = "mainnet"): string {
  let cursor = tipHeight;
  let seconds = 0;
  for (const [start, spacing] of spacingEras(network).reverse()) {
    if (cursor < start || cursor <= height) continue;
    const lower = Math.max(height, start - 1);
    seconds += (cursor - lower) * spacing;
    cursor = lower;
  }
  return unixToYmd(nowUnix() - seconds);
}

export function ymdDaysAgo(days: number): string {
  return unixToYmd(nowUnix() - days * 86_400);
}

/** Validate syntax before clearing a recovery phrase or making a network request.
 * Empty and `auto` are valid SDK defaults; restore forms should require an explicit value.
 */
export function validateBirthdayInput(raw: string | number): void {
  const s = String(raw).trim();
  if (typeof raw === "string" && (!s || s.toLowerCase() === "auto")) return;
  if (typeof raw === "number" || /^\d+$/.test(s)) {
    const height = Number(raw);
    if (!Number.isSafeInteger(height) || height < 1 || height > 0xffff_ffff) {
      throw new WalletError("invalid_birthday", "invalid birthday height");
    }
    return;
  }
  ymdToUnix(s);
}

/** Digits are an exact height; dates estimate a height with a safety margin.
 * Empty / `auto` defaults to tip minus 100.
 */
export function parseBirthdayInput(raw: string, tipHeight: number, network: BirthdayNetwork = "mainnet"): number {
  validateBirthdayInput(raw);
  const s = raw.trim();
  if (!s || s.toLowerCase() === "auto") return Math.max(1, tipHeight - 100);
  if (/^\d+$/.test(s)) {
    const h = Number(s);
    if (!Number.isInteger(h) || h < 1) throw new Error("birthday height is not a number");
    return h;
  }
  // Dates are UTC days; allow one day for the user's timezone.
  if (ymdToUnix(s) > nowUnix() + 86_400) {
    throw new WalletError("birthday_above_tip", "birthday date is in the future");
  }
  return heightBeforeSeconds(heightFromDate(s, tipHeight, network), DATE_SAFETY_BLOCKS * BLOCK_SECONDS, network);
}
