import { checkSignal, type WalletTransaction } from "./wallet-storage";

export const INCREMENTAL_FORMAT = "z-stack-snapshot-ref-3";
export const MAX_CHUNK_BYTES = 32 * 1024;
const MIN_CHUNK_BYTES = 8 * 1024;
const INDEX_PAGE_SIZE = 128;

type Chunk = { key: string; length: number; hash: number };
type PlannedChunk = Omit<Chunk, "key"> & { offset: number };
export type ChunkIndex = { key: string; byteLength: number; chunkCount: number };

// Deterministic gear table for content-defined boundaries, not cryptography.
// Fingerprints only select candidates; reuse ALWAYS requires exact byte equality.
const gear = Uint32Array.from({ length: 256 }, (_, i) => {
  let value = i + 1;
  value ^= value << 13; value ^= value >>> 17; value ^= value << 5;
  return Math.imul(value, 0x9e3779b1) >>> 0;
});

type ChunkScan = { offset: number; boundary: number; hash: number };

// Keep the hot byte loop synchronous; the outer planner yields between batches.
function scanBatch(bytes: Uint8Array, start: number, end: number, state: ChunkScan, chunks: PlannedChunk[]): void {
  let { offset, boundary, hash } = state;
  for (let i = start; i < end; i++) {
    boundary = ((boundary << 1) + gear[bytes[i]]) >>> 0;
    hash = Math.imul(hash ^ bytes[i], 0x01000193) >>> 0;
    const length = i + 1 - offset;
    if (length === MAX_CHUNK_BYTES || i + 1 === bytes.length
      || length >= MIN_CHUNK_BYTES && (boundary & 0x1fff) === 0) {
      chunks.push({ offset, length, hash });
      offset = i + 1; boundary = 0; hash = 0x811c9dc5;
    }
  }
  state.offset = offset; state.boundary = boundary; state.hash = hash;
}

/** Stable boundaries resynchronize after insertions/deletions in opaque Rust bytes. */
export async function planSnapshotChunks(bytes: Uint8Array, signal?: AbortSignal): Promise<PlannedChunk[]> {
  const chunks: PlannedChunk[] = [];
  const state = { offset: 0, boundary: 0, hash: 0x811c9dc5 };
  const batch = 1024 * 1024;
  for (let start = 0; start < bytes.length; start += batch) {
    checkSignal(signal);
    scanBatch(bytes, start, Math.min(start + batch, bytes.length), state, chunks);
    if (start + batch < bytes.length) {
      // Planning precedes the IDB transaction. Yield between bounded scans so
      // large wallets do not monopolize the UI or delay Forget/cancellation.
      const scheduler = (globalThis as typeof globalThis & { scheduler?: { yield?: () => Promise<void> } }).scheduler;
      if (scheduler?.yield) await scheduler.yield();
      else await new Promise<void>(resolve => setTimeout(resolve, 0));
    }
  }
  checkSignal(signal);
  return chunks;
}

export function validChunkCount(ref: ChunkIndex): boolean {
  return Number.isSafeInteger(ref.chunkCount)
    && ref.chunkCount >= Math.ceil(ref.byteLength / MAX_CHUNK_BYTES)
    && ref.chunkCount <= Math.ceil(ref.byteLength / MIN_CHUNK_BYTES);
}

function indexKey(key: string, page: number): string {
  return page === 0 ? key : `${key}:i:${page}`;
}

function incomplete(): never {
  throw new Error("Saved wallet snapshot is incomplete; keep the record and retry recovery");
}

/** Each index page is bounded too; no ever-growing manifest/large IDB value. */
function readIndex<T>(tx: WalletTransaction<T>, ref: ChunkIndex, done: (chunks: Chunk[]) => void): void {
  const chunks: Chunk[] = new Array(ref.chunkCount);
  const count = Math.ceil(ref.chunkCount / INDEX_PAGE_SIZE);
  let remaining = count;
  for (let page = 0; page < count; page++) {
    tx.request(tx.store.get(indexKey(ref.key, page)), value => {
      const start = page * INDEX_PAGE_SIZE;
      if (!Array.isArray(value) || value.length !== Math.min(INDEX_PAGE_SIZE, ref.chunkCount - start)) incomplete();
      value.forEach((chunk: Chunk, i) => {
        if (!chunk || typeof chunk.key !== "string"
          || !/^snapshot:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}:c:\d{1,10}$/.test(chunk.key)
          || !Number.isSafeInteger(chunk.length) || chunk.length < 1 || chunk.length > MAX_CHUNK_BYTES
          || !Number.isInteger(chunk.hash) || chunk.hash < 0 || chunk.hash > 0xffffffff) incomplete();
        chunks[start + i] = chunk;
      });
      if (--remaining === 0) {
        if (chunks.reduce((length, chunk) => length + chunk.length, 0) !== ref.byteLength) incomplete();
        done(chunks);
      }
    });
  }
}

function bytesOf(value: unknown): Uint8Array | null {
  return value instanceof Uint8Array ? value : value instanceof ArrayBuffer ? new Uint8Array(value) : null;
}

export function readIncrementalSnapshot<T>(tx: WalletTransaction<T>, ref: ChunkIndex, done: (bytes: Uint8Array) => void): void {
  readIndex(tx, ref, chunks => {
    const bytes = new Uint8Array(ref.byteLength);
    let offset = 0, remaining = chunks.length;
    for (const chunk of chunks) {
      const start = offset;
      offset += chunk.length;
      tx.request(tx.store.get(chunk.key), value => {
        const payload = bytesOf(value);
        if (!payload || payload.length !== chunk.length) incomplete();
        bytes.set(payload, start);
        if (--remaining === 0) done(bytes);
      });
    }
  });
}

/** Queue only changed payloads, then replace index/preview and retire unused data atomically. */
export function writeIncrementalSnapshot<T>(
  tx: WalletTransaction<T>, bytes: Uint8Array, plan: PlannedChunk[], key: string,
  previous: ChunkIndex | undefined, commit: () => void,
): void {
  const write = (old: Chunk[]) => {
    const candidates = new Map<string, Chunk>();
    for (const chunk of old) candidates.set(`${chunk.length}:${chunk.hash}`, chunk);
    const next: Chunk[] = new Array(plan.length);
    const reads = new Map<string, { candidate: Chunk; indices: number[] }>();
    const put = (chunk: PlannedChunk, i: number) => {
      const chunkKey = `${key}:c:${i}`;
      tx.store.put(bytes.slice(chunk.offset, chunk.offset + chunk.length), chunkKey);
      next[i] = { key: chunkKey, length: chunk.length, hash: chunk.hash };
    };
    plan.forEach((chunk, i) => {
      const candidate = candidates.get(`${chunk.length}:${chunk.hash}`);
      if (!candidate) { put(chunk, i); return; }
      const pending = reads.get(candidate.key);
      if (pending) pending.indices.push(i);
      else reads.set(candidate.key, { candidate, indices: [i] });
    });
    const finish = () => {
      const retained = new Set(next.map(chunk => chunk.key));
      for (const oldKey of new Set(old.map(chunk => chunk.key))) {
        if (!retained.has(oldKey)) tx.store.delete(oldKey);
      }
      if (previous) for (let page = 0; page < Math.ceil(previous.chunkCount / INDEX_PAGE_SIZE); page++) {
        tx.store.delete(indexKey(previous.key, page));
      }
      for (let i = 0; i < next.length; i += INDEX_PAGE_SIZE) {
        tx.store.put(next.slice(i, i + INDEX_PAGE_SIZE), indexKey(key, i / INDEX_PAGE_SIZE));
      }
      commit();
    };
    let remaining = reads.size;
    if (!remaining) { finish(); return; }
    for (const { candidate, indices } of reads.values()) {
      tx.request(tx.store.get(candidate.key), value => {
        const stored = bytesOf(value);
        for (const i of indices) {
          const chunk = plan[i];
          // A missing, damaged, or colliding candidate is replaced from the
          // engine's complete snapshot; it is never trusted by fingerprint.
          let equal = stored?.length === chunk.length;
          for (let j = 0; equal && j < chunk.length; j++) equal = stored![j] === bytes[chunk.offset + j];
          if (equal) next[i] = candidate;
          else put(chunk, i);
        }
        if (--remaining === 0) finish();
      });
    }
  };
  if (previous) readIndex(tx, previous, write);
  else write([]);
}
