/**
 * Unified, fail-closed wallet privacy policy for `@z-stack/sdk`.
 *
 * Key guarantees:
 * - Default memo policy with PIR is `"pir-only"`: pure-Ironwood notes recover privately
 *   via Enhance PIR; non-PIR transactions never trigger exact-height or exact-txid fetches
 *   automatically.
 * - Explicit one-off retrieval for an out-of-PIR transaction uses an aligned cover bucket
 *   (default 10 blocks, never 1 block when chain depth allows), validated in Rust via
 *   `applySharedMemos`.
 * - Default transparent policy is `"compact-plus-pir"` (all-pool compact blocks for
 *   birthday..tip + Transparent PIR v12 for pre-birthday history) — zero `GetAddressUtxos`.
 */

import { WalletError } from "@z-stack/core";

export type MemoPolicy =
  | "pir-only"
  | "pir-plus-shared-tip"
  | "shared-sequential"
  | "on-demand"
  | "auto"
  | "shared";

/**
 * Compute an aligned, inclusive cover bucket `[start, end]` covering `height` without
 * revealing an exact single block height to the gateway.
 */
export function alignedCoverBucket(
  height: number,
  tip: number,
  bucketWidth = 10,
): { start: number; end: number } {
  if (!Number.isSafeInteger(height) || height < 1 || !Number.isSafeInteger(tip) || tip < height) {
    throw new WalletError("unknown", "Invalid mined height for cover-bucket retrieval.");
  }
  if (!Number.isSafeInteger(bucketWidth) || bucketWidth < 2 || bucketWidth > 1_000) {
    throw new WalletError("unknown", "Invalid cover bucket size; expected an integer in 2..=1000.");
  }
  const width = Math.trunc(bucketWidth);
  if (tip <= width) {
    return { start: 1, end: tip };
  }
  const alignedStart = Math.floor((height - 1) / width) * width + 1;
  const alignedEnd = alignedStart + width - 1;
  if (alignedEnd <= tip) {
    return { start: alignedStart, end: alignedEnd };
  }
  // Near chain tip: shift the full window backwards so we NEVER shrink to `(h, h)`.
  return { start: tip - width + 1, end: tip };
}

/**
 * Normalize legacy `memoFetch` strings onto the unified privacy-preserving modes.
 *
 * Note: `"auto"` with an Enhance PIR transport maps to `"pir-only"` so Enhance PIR
 * is NEVER undone by single-block or txid fallback fetches.
 */
export function resolveMemoPolicy(
  requested: MemoPolicy | undefined,
  hasEnhancePir: boolean,
): "pir-only" | "pir-plus-shared-tip" | "shared-sequential" | "on-demand" | "auto" {
  if (requested === undefined) {
    return hasEnhancePir ? "pir-only" : "on-demand";
  }
  if (requested === "shared") return "shared-sequential";
  if (requested === "auto" && hasEnhancePir) return "pir-only";
  if (
    requested === "pir-only" ||
    requested === "pir-plus-shared-tip" ||
    requested === "shared-sequential" ||
    requested === "on-demand" ||
    requested === "auto"
  ) {
    return requested;
  }
  throw new WalletError("unknown", `Unsupported memoFetch policy: ${String(requested)}`);
}
