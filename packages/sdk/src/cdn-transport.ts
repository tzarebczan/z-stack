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

export const DEFAULT_SEGMENT_BLOCKS = 1_000;
export const DEFAULT_REORG_DEPTH = 100;
export const DEFAULT_CDN_CONCURRENCY = 6;

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
  const fetcher = options.fetch ?? globalThis.fetch;

  /** Fetch a single segment from CDN, falling back to live transport if unavailable */
  async function fetchSegment(segStart: number, signal?: AbortSignal): Promise<Uint8Array> {
    const segEnd = segStart + segmentBlocks - 1;
    const url = `${cdnBase}/compact-blocks/${network}/${segStart}.bin`;

    try {
      signal?.throwIfAborted();
      const res = await fetcher(url, {
        ...transportFetchOptions,
        signal,
        headers: { Accept: "application/octet-stream" },
      });

      if (res.ok) {
        const buf = await res.arrayBuffer();
        signal?.throwIfAborted();
        const bytes = new Uint8Array(buf);
        if (bytes.byteLength > 0) {
          return bytes;
        }
      }
    } catch (err) {
      signal?.throwIfAborted();
      // On CDN fetch error (404, network error, etc.), proceed to fallback
    }

    // Fallback to live transport for this segment
    return live.blocks(segStart, segEnd, signal);
  }

  /** Fetch a historical range [start..end] via CDN segments */
  async function fetchHistoricalRange(
    start: number,
    end: number,
    signal?: AbortSignal,
  ): Promise<Uint8Array> {
    const startSeg = Math.floor(start / segmentBlocks) * segmentBlocks;
    const endSeg = Math.floor(end / segmentBlocks) * segmentBlocks;

    const segStarts: number[] = [];
    for (let s = startSeg; s <= endSeg; s += segmentBlocks) {
      segStarts.push(s);
    }

    // Fetch required segments with bounded concurrency
    const segments = await mapConcurrent(segStarts, concurrency, (segStart) =>
      fetchSegment(segStart, signal),
    );

    const parts: Uint8Array[] = [];
    for (let i = 0; i < segStarts.length; i++) {
      const segStart = segStarts[i];
      const segEnd = segStart + segmentBlocks - 1;
      const rawSeg = segments[i];

      const needSlice = start > segStart || end < segEnd;
      if (!needSlice) {
        parts.push(rawSeg);
      } else {
        const subStart = Math.max(start, segStart);
        const subEnd = Math.min(end, segEnd);
        const sliced = sliceSegmentFrames(rawSeg, segStart, subStart, subEnd);
        parts.push(sliced);
      }
    }

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
          fetchHistoricalRange(start, safeTip, signal),
          live.blocks(safeTip + 1, end, signal),
        ]);
        return concatBytes([historicalBytes, liveBytes]);
      }

      // Entirely within the historical CDN segment zone
      return fetchHistoricalRange(start, end, signal);
    },
  };
}
