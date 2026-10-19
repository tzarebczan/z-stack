/**
 * High-speed CDN segment transport for compact blocks.
 *
 * Downloads finalized historical compact blocks as 1,000-block immutable
 * segments from edge CDN caches with parallel HTTP/2 concurrency.
 * Blocks within chain reorg depth (the dynamic tip) and sensitive operations
 * (submit, treeState, subtreeRoots, PIR) are routed dynamically to the live transport.
 */

import type { BlockTransport } from "./lwd";
import { transportFetchOptions } from "./lwd";
import type { Network } from "./runtime";
import { COMPACT_RANGE_MAX_BYTES } from "./transport-bounds";

export const DEFAULT_SEGMENT_BLOCKS = 1_000;
export const DEFAULT_REORG_DEPTH = 100;
export const DEFAULT_CDN_CONCURRENCY = 6;
const CDN_BODY_MAX = COMPACT_RANGE_MAX_BYTES;
const CDN_FRAME_MAX = 4 * 1024 * 1024;
const CDN_TIMEOUT_MS = 60_000;

class CdnSegmentError extends Error {}

/** A ref'd deadline. AbortSignal.timeout() does not keep the event loop alive. */
function startDeadline(ms: number): { signal: AbortSignal; stop: () => void } {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new DOMException("cdn segment timed out", "TimeoutError")), ms);
  return {
    signal: controller.signal,
    stop() {
      clearTimeout(timer);
    },
  };
}

export interface CdnTransportOptions {
  /** Base URL of the CDN, e.g. "https://cloud.cofferwallet.com" */
  cdnUrl: string;
  /** Network name (mainnet, testnet, regtest) */
  network: Network;
  /** Live fallback transport for tip, subtreeRoots, treeState, submit, tx, and near-tip blocks */
  live: BlockTransport;
  /** Segment size in blocks. Must be a positive integer, default 1,000. */
  segmentBlocks?: number;
  /** Reorg safety depth where CDN segments stop and live transport takes over. Default 100. */
  reorgDepth?: number;
  /** Maximum concurrent segment downloads. Default 6. */
  concurrency?: number;
  /** Per-request deadline before the same range is read from the live transport. Default 60s. */
  segmentTimeoutMs?: number;
  /** Custom fetch implementation. Defaults to global fetch. */
  fetch?: typeof fetch;
}

function concatBytes(chunks: Uint8Array[]): Uint8Array {
  if (chunks.length === 0) return new Uint8Array(0);
  if (chunks.length === 1) return chunks[0];
  let total = 0;
  for (const c of chunks) total += c.length;
  const out = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

/**
 * Slice delimited compact-block frames for the requested sub-range [reqStart..reqEnd].
 * Frames are wire format: 4-byte big-endian message length followed by the protobuf bytes.
 */
export function sliceSegmentFrames(
  segment: Uint8Array,
  segStart: number,
  reqStart: number,
  reqEnd: number,
): Uint8Array {
  if (reqStart > reqEnd) return new Uint8Array(0);
  const view = new DataView(segment.buffer, segment.byteOffset, segment.byteLength);
  let offset = 0;
  let currentHeight = segStart;
  let sliceStart = -1;
  let sliceEnd = -1;

  while (offset < segment.byteLength) {
    if (offset + 4 > segment.byteLength) {
      throw new Error(`corrupted segment at height ${currentHeight}: truncated length header`);
    }
    const msgLen = view.getUint32(offset, false);
    const frameLen = 4 + msgLen;
    if (offset + frameLen > segment.byteLength) {
      throw new Error(`corrupted segment at height ${currentHeight}: truncated frame data`);
    }

    if (currentHeight >= reqStart && sliceStart === -1) {
      sliceStart = offset;
    }
    if (currentHeight <= reqEnd) {
      sliceEnd = offset + frameLen;
    }
    if (currentHeight > reqEnd) {
      break;
    }

    offset += frameLen;
    currentHeight++;
  }

  if (sliceStart === -1 || sliceEnd === -1 || sliceEnd < sliceStart) {
    return new Uint8Array(0);
  }
  return segment.subarray(sliceStart, sliceEnd);
}

/** Count length-delimited frames. A short or torn object is not a usable segment. */
function countCompleteFrames(segment: Uint8Array): number {
  const view = new DataView(segment.buffer, segment.byteOffset, segment.byteLength);
  let offset = 0;
  let frames = 0;
  while (offset < segment.byteLength) {
    if (offset + 4 > segment.byteLength) throw new CdnSegmentError("truncated length header");
    const msgLen = view.getUint32(offset, false);
    if (msgLen > CDN_FRAME_MAX) throw new CdnSegmentError("cdn frame exceeds size limit");
    const frameLen = 4 + msgLen;
    if (offset + frameLen > segment.byteLength) throw new CdnSegmentError("truncated frame data");
    offset += frameLen;
    frames++;
  }
  return frames;
}

/** Stop before a declared or streaming body can exceed the compact-range ceiling. */
async function readBoundedBody(response: Response, signal: AbortSignal, maxBytes: number): Promise<Uint8Array> {
  const declared = response.headers.get("content-length");
  if (declared !== null && (!/^\d+$/.test(declared.trim()) || Number(declared.trim()) > maxBytes)) {
    await response.body?.cancel().catch(() => {});
    throw new CdnSegmentError("cdn segment exceeds size limit");
  }
  if (!response.body) throw new CdnSegmentError("cdn segment has no body");
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  let onAbort: (() => void) | undefined;
  const aborted = new Promise<never>((_, reject) => {
    onAbort = () => reject(signal.reason);
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) onAbort();
  });
  void aborted.catch(() => {});
  try {
    for (;;) {
      signal.throwIfAborted();
      const step = await Promise.race([reader.read(), aborted]);
      signal.throwIfAborted();
      if (step.done) break;
      if (step.value.byteLength > maxBytes - total) throw new CdnSegmentError("cdn segment exceeds size limit");
      total += step.value.byteLength;
      if (step.value.byteLength) chunks.push(step.value);
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    if (onAbort) signal.removeEventListener("abort", onAbort);
    reader.releaseLock();
  }
  return concatBytes(chunks);
}

/** Bounded concurrency queue for parallel fetching */
async function mapConcurrent<T, R>(
  items: T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<R[]> {
  const results = new Array<R>(items.length);
  let currentIndex = 0;

  async function worker(): Promise<void> {
    while (currentIndex < items.length) {
      const idx = currentIndex++;
      results[idx] = await fn(items[idx]);
    }
  }

  const workers = Array.from(
    { length: Math.min(limit, items.length) },
    () => worker(),
  );
  await Promise.all(workers);
  return results;
}

/**
 * Creates a BlockTransport that streams historical blocks in bulk from edge CDN segments,
 * falling back gracefully to the live transport for recent tip blocks and any cache misses.
 */
export function cdnSegmentTransport(options: CdnTransportOptions): BlockTransport {
  const cdnBase = options.cdnUrl.replace(/\/+$/, "");
  const network = options.network;
  const live = options.live;
  const segmentBlocks = Math.max(1, Math.floor(options.segmentBlocks ?? DEFAULT_SEGMENT_BLOCKS));
  const reorgDepth = Math.max(0, Math.floor(options.reorgDepth ?? DEFAULT_REORG_DEPTH));
  const concurrency = Math.max(1, Math.floor(options.concurrency ?? DEFAULT_CDN_CONCURRENCY));
  const segmentTimeoutMs = Math.max(1, Math.floor(options.segmentTimeoutMs ?? CDN_TIMEOUT_MS));
  const fetcher = options.fetch ?? globalThis.fetch;

  /** Fetch a segment range from CDN, falling back to live transport for [subStart..subEnd] if unavailable */
  async function fetchSegmentRange(
    segStart: number,
    subStart: number,
    subEnd: number,
    currentTip?: number,
    signal?: AbortSignal,
  ): Promise<Uint8Array> {
    // If the segment ends beyond the current known chain tip, the 1,000-block file
    // cannot be fully mined yet and won't exist on CDN. Route live directly.
    if (typeof currentTip === "number" && segStart + segmentBlocks - 1 > currentTip) {
      return live.blocks(subStart, subEnd, signal);
    }

    const url = `${cdnBase}/compact-blocks/${network}/${segStart}.bin`;
    const segEnd = segStart + segmentBlocks - 1;

    const MAX_CDN_RETRIES = 2;
    for (let attempt = 0; attempt <= MAX_CDN_RETRIES; attempt++) {
      const deadline = startDeadline(segmentTimeoutMs);
      const attemptSignal = signal ? AbortSignal.any([signal, deadline.signal]) : deadline.signal;
      try {
        signal?.throwIfAborted();
        const res = await fetcher(url, {
          ...transportFetchOptions,
          signal: attemptSignal,
          headers: { Accept: "application/octet-stream" },
        });

        if (res.ok) {
          const bytes = await readBoundedBody(res, attemptSignal, CDN_BODY_MAX);
          signal?.throwIfAborted();
          // A 200 that ends on a frame boundary can still omit blocks. Accept
          // the object only when it is the whole segment, then slice.
          if (countCompleteFrames(bytes) !== segmentBlocks) {
            throw new CdnSegmentError(`cdn segment ${segStart} does not cover ${segmentBlocks} blocks`);
          }
          const needSlice = subStart > segStart || subEnd < segEnd;
          const sliced = needSlice ? sliceSegmentFrames(bytes, segStart, subStart, subEnd) : bytes;
          if (countCompleteFrames(sliced) !== subEnd - subStart + 1) {
            throw new CdnSegmentError(`cdn segment ${segStart} does not cover ${subStart}..${subEnd}`);
          }
          return sliced;
        }
        await res.body?.cancel().catch(() => {});
        // 404 and other non-transient statuses fall back immediately.
        if (res.status === 404 || res.status < 500 || attempt >= MAX_CDN_RETRIES) break;
      } catch (error) {
        signal?.throwIfAborted();
        const message = error instanceof Error ? error.message : "";
        const giveUp = error instanceof CdnSegmentError
          || deadline.signal.aborted
          || (error instanceof Error && error.name === "AbortError")
          || /size limit/.test(message);
        if (giveUp || attempt >= MAX_CDN_RETRIES) break;
      } finally {
        deadline.stop();
      }
      signal?.throwIfAborted();
      await new Promise((resolve) => setTimeout(resolve, 300 * 2 ** attempt));
    }

    // Fallback to live transport for the exact requested sub-range
    return live.blocks(subStart, subEnd, signal);
  }

  /** Fetch a historical range [start..end] via CDN segments */
  async function fetchHistoricalRange(
    start: number,
    end: number,
    currentTip?: number,
    signal?: AbortSignal,
  ): Promise<Uint8Array> {
    const startSeg = Math.floor(start / segmentBlocks) * segmentBlocks;
    const endSeg = Math.floor(end / segmentBlocks) * segmentBlocks;

    type RangeItem = { segStart: number; subStart: number; subEnd: number };
    const items: RangeItem[] = [];
    for (let s = startSeg; s <= endSeg; s += segmentBlocks) {
      items.push({
        segStart: s,
        subStart: Math.max(start, s),
        subEnd: Math.min(end, s + segmentBlocks - 1),
      });
    }

    // Fetch required segments with bounded concurrency
    const parts = await mapConcurrent(items, concurrency, (item) =>
      fetchSegmentRange(item.segStart, item.subStart, item.subEnd, currentTip, signal),
    );

    return concatBytes(parts);
  }

  return {
    ...live,
    kind: `cdn-segment:${live.kind}`,
    label: `CDN Segment (${live.label})`,

    blocks: async (start, end, signal) => {
      if (start > end) return new Uint8Array(0);

      // Determine the chain tip to identify the safe reorg boundary
      const currentTip = await live.tip(signal);
      signal?.throwIfAborted();
      const safeTip = Math.max(0, currentTip - reorgDepth);

      if (start > safeTip) {
        // Entirely within the live tip zone
        return live.blocks(start, end, signal);
      }

      if (end > safeTip) {
        // Range spans both historical CDN segments and the dynamic live tip
        const [historicalBytes, liveBytes] = await Promise.all([
          fetchHistoricalRange(start, safeTip, currentTip, signal),
          live.blocks(safeTip + 1, end, signal),
        ]);
        return concatBytes([historicalBytes, liveBytes]);
      }

      // Entirely within the historical CDN segment zone
      return fetchHistoricalRange(start, end, currentTip, signal);
    },
  };
}
