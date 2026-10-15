import type { WalletSnapshot } from "@z-stack/core";
import { compactBlockRequestLimit, sharedMemoBlockRequestLimit, type BlockTransport } from "./lwd";
import type { ScanSession } from "./scan-host";
import { abortable } from "./abort";

export type PublicDataStatus = NonNullable<WalletSnapshot["sharedMemoStatus"]>;

/** Pool selection first appeared in protocol v0.5. Older servers may ignore it. */
export function supportsTransparentCompact(version?: string): boolean {
  const match = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(version ?? "");
  return !!match && (Number(match[1]) > 0 || Number(match[2]) >= 5);
}

/** One bounded pass. Ranges depend on coverage, never on wallet matches. */
export async function syncPublicData({ source, transport, transparent, memos, signal, assertCurrent, checkpoint, maxBlocksPerRequest, maxSharedMemoBlocksPerRequest, maxSharedMemoBlocksPerPass = 100 } : {
  source: ScanSession;
  transport: BlockTransport;
  maxBlocksPerRequest?: number;
  maxSharedMemoBlocksPerRequest?: number;
  /** Fixed wallet-independent work budget. Manual callers retain the 100-block default. */
  maxSharedMemoBlocksPerPass?: number;
  transparent: boolean;
  memos: boolean;
  signal: AbortSignal;
  assertCurrent: () => void;
  checkpoint: () => Promise<void>;
}): Promise<{ transparent?: PublicDataStatus; memos?: PublicDataStatus }> {
  const compactLimit = compactBlockRequestLimit(maxBlocksPerRequest);
  const memoLimit = sharedMemoBlockRequestLimit(maxSharedMemoBlocksPerRequest);
  if (!Number.isSafeInteger(maxSharedMemoBlocksPerPass) || maxSharedMemoBlocksPerPass < 1 || maxSharedMemoBlocksPerPass > 100) {
    throw new Error("maxSharedMemoBlocksPerPass must be an integer from 1 to 100");
  }
  const status: { transparent?: PublicDataStatus; memos?: PublicDataStatus } = {};
  const current = () => { signal.throwIfAborted(); assertCurrent(); };
  const snapshot = async () => {
    const value = JSON.parse(await source.snapshotJson("")) as WalletSnapshot;
    current();
    return value;
  };
  const state = await snapshot();
  const tip = state.scannedHeight ?? 0;
  if (transparent) {
    const supported = source.applyTransparentBlocks && (await source.supportsTransparentBlocks?.() ?? true);
    current();
    if (!supported) status.transparent = "unsupported";
    else {
      // Persist the requirement before network I/O: a migrated snapshot must not
      // spend address-lookup UTXOs while compact coverage is missing.
      // Existing coverage was produced by applyTransparentBlocks, which already
      // set the requirement. Reapplying an empty range dirties an unchanged wallet.
      if (state.transparentScanHeight == null) await source.applyTransparentBlocks?.(new Uint8Array());
      current();
      await checkpoint();
      current();
      const info = await abortable(signal, async () => transport.info?.(signal));
      current();
      if (!transport.transparentBlocks || !source.applyTransparentBlocks ||
          !info?.transparentCompact || !supportsTransparentCompact(info.protocolVersion)) {
        status.transparent = "unsupported";
      } else {
        let next = (state.transparentScanHeight ?? (state.birthdayHeight - 1)) + 1;
        // Backfill old snapshots without blocking every sync until birthday..tip completes.
        const last = Math.min(tip, next + 19_999);
        let nextCheckpoint = next + 5000;
        while (next <= last) {
          const end = Math.min(last, next + Math.min(1000, compactLimit) - 1);
          const bytes = await abortable(signal, () => transport.transparentBlocks!(next, end, signal));
          current();
          await source.applyTransparentBlocks(bytes);
          current();
          const after = await snapshot();
          if (after.transparentScanHeight !== end) throw new Error("Incomplete transparent range");
          next = end + 1;
          if (next > last || next >= nextCheckpoint) {
            while (nextCheckpoint <= next) nextCheckpoint += 5000;
            await checkpoint();
            current();
          }
        }
        status.transparent = next > tip ? "complete" : "scanning";
      }
    }
  }
  if (memos) {
    if (!transport.sharedMemos || !source.applySharedMemos) status.memos = "unsupported";
    else {
      let next = (state.memoScanHeight ?? (state.birthdayHeight - 1)) + 1;
      // The final apply may have succeeded in memory before a checkpoint failed.
      // A retry at tip must make that state durable before reporting completion.
      if (next > tip) {
        await checkpoint();
        current();
      }
      const last = Math.min(tip, next + maxSharedMemoBlocksPerPass - 1);
      while (next <= last) {
        // After the birthday range, boundaries are shared by every wallet.
        const end = Math.min(last, next + memoLimit - 1, Math.floor(next / 10) * 10 + 9);
        const json = await abortable(signal, () => transport.sharedMemos!(next, end, signal));
        current();
        if (json === null) { status.memos = "unsupported"; break; }
        // Bind the response to this request before it can update coverage.
        const bundle = JSON.parse(json) as { start?: number; end?: number };
        if (bundle.start !== next || bundle.end !== end) throw new Error("Wrong shared memo range");
        await source.applySharedMemos(json);
        current();
        const after = await snapshot();
        if (after.memoScanHeight !== end) throw new Error("Incomplete shared memo range");
        await checkpoint();
        current();
        next = end + 1;
      }
      status.memos ??= next > tip ? "complete" : "scanning";
    }
  }
  return status;
}
