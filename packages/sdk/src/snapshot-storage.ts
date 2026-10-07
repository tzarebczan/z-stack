import { WalletError, type UnlockPolicy, type WalletSnapshot } from "@z-stack/core";
import { aborted, abortable, generationValue, readWalletGeneration, WALLET_GENERATION_KEY, walletTransaction, walletStorageAvailable, type WalletGeneration, type WalletRecordStore, type WalletTransaction } from "./wallet-storage";
import { invalidateVaultOperations } from "./vault-operation";
import { INCREMENTAL_FORMAT, MAX_CHUNK_BYTES, planSnapshotChunks, readIncrementalSnapshot, validChunkCount, writeIncrementalSnapshot, type ChunkIndex } from "./snapshot-chunks";

export { openWalletDb, readWalletGeneration, advanceWalletGeneration, type WalletGeneration } from "./wallet-storage";

const DEFAULT = "default";
const FORMAT = "z-stack-snapshot-ref-1";
const CHUNKED_FORMAT = "z-stack-snapshot-ref-2";
// Stay below Chromium's large-value externalization threshold, including in
// private contexts. Large Blob and typed-array records can lose their backing
// after repeated replacements. Every chunk and its manifest commit atomically.
const SNAPSHOT_CHUNK_BYTES = MAX_CHUNK_BYTES;
const PREFIX = "snapshot:";

type SnapshotRef = {
  format: typeof FORMAT | typeof CHUNKED_FORMAT | typeof INCREMENTAL_FORMAT;
  chunkCount?: number;
  key: string;
  byteLength: number;
  preview: WalletSnapshot;
  generation?: WalletGeneration;
};

function chunkKey(key: string, index: number): string {
  return index === 0 ? key : `${key}:${index}`;
}

const pendingWrites = new Set<() => void>();

export function abortSnapshotWrites(): void {
  for (const abort of pendingWrites) abort();
}

function snapshotRef(value: unknown): SnapshotRef | null {
  if (!value || typeof value !== "object") return null;
  const ref = value as SnapshotRef;
  return (ref.format === FORMAT || ref.format === CHUNKED_FORMAT || ref.format === INCREMENTAL_FORMAT && validChunkCount(ref as ChunkIndex)) && typeof ref.key === "string" && ref.key.startsWith(PREFIX)
    && (ref.generation === undefined || typeof ref.generation === "string" && !!ref.generation)
    && Number.isSafeInteger(ref.byteLength) && ref.byteLength > 0
    && typeof ref.preview?.unifiedAddress === "string" && !!ref.preview.unifiedAddress
    && ["mainnet", "testnet", "regtest"].includes(ref.preview.network)
    && Number.isSafeInteger(ref.preview.birthdayHeight) && ref.preview.birthdayHeight > 0
    && Number.isSafeInteger(ref.preview.balance?.totalAvailable) && ref.preview.balance.totalAvailable >= 0
    ? ref : null;
}

function sameBytes(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) return false;
  for (let i = 0; i < left.byteLength; i++) {
    if (left[i] !== right[i]) return false;
  }
  return true;
}

function snapshotBytes(value: unknown): Uint8Array | null {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (!(value instanceof Uint8Array)) return null;
  return value.byteOffset === 0 && value.byteLength === value.buffer.byteLength ? value : value.slice();
}

type SnapshotPayload = Uint8Array | Blob;

/** A read-only replacement baseline; identity changes only with the new snapshot. */
export type SnapshotReplacement = {
  generation: WalletGeneration;
  previous: WalletGeneration;
  previousKey: string | null;
  previousLegacy?: SnapshotPayload | null;
};

function isBlob(value: unknown): value is Blob {
  return typeof Blob !== "undefined" && value instanceof Blob;
}

function snapshotPayload(value: unknown): SnapshotPayload | null {
  return isBlob(value) ? value : snapshotBytes(value);
}

const GENERATION_MISMATCH = Symbol("interrupted wallet replacement");

/** Repair an earlier interrupted claim without erasing its snapshot or pending sends. */
async function readCurrentSnapshot<T>(
  read: (tx: WalletTransaction<T | typeof GENERATION_MISMATCH>, saved: unknown, generation: WalletGeneration) => void,
  signal?: AbortSignal,
): Promise<T> {
  const run = () => walletTransaction<T | typeof GENERATION_MISMATCH>("readonly", tx => {
    tx.request(tx.store.get(WALLET_GENERATION_KEY), value => {
      const generation = generationValue(value);
      tx.request(tx.store.get(DEFAULT), saved => {
        const ref = snapshotRef(saved);
        if ((ref || snapshotPayload(saved)) && generationValue(ref?.generation) !== generation) {
          tx.result(GENERATION_MISMATCH);
        } else read(tx, saved, generation);
      });
    });
  }, signal);
  let result = await run();
  if (result === GENERATION_MISMATCH) {
    await walletTransaction<void>("readwrite", tx => {
      tx.request(tx.store.get(DEFAULT), saved => {
        const ref = snapshotRef(saved);
        if (!ref && !snapshotPayload(saved)) return; // Forget has already removed it.
        const generation = generationValue(ref?.generation);
        if (generation === generationValue(undefined)) tx.store.delete(WALLET_GENERATION_KEY);
        else tx.store.put(generation, WALLET_GENERATION_KEY);
      });
    }, signal);
    result = await run();
  }
  if (result === GENERATION_MISMATCH) throw new WalletError("wallet_db", "Saved wallet identity is inconsistent. Keep its data and retry recovery.");
  return result;
}

export async function prepareSnapshotReplacement(
  signal: AbortSignal, previous: WalletGeneration, allowReplace: boolean,
): Promise<SnapshotReplacement> {
  if (!walletStorageAvailable()) return { generation: previous, previous, previousKey: null };
  return readCurrentSnapshot<SnapshotReplacement>((tx, saved, generation) => {
    if (generation !== previous) throw new WalletError("wallet_changed", "saved wallet changed; retry the operation");
    if (!allowReplace && saved != null) throw new WalletError("already_exists", "A saved wallet exists. Confirm replacement explicitly.");
    tx.result({ generation: crypto.randomUUID(), previous, previousKey: snapshotRef(saved)?.key ?? null,
      previousLegacy: snapshotRef(saved) ? null : snapshotPayload(saved) });
  }, signal);
}

/** Keep preview size independent of history, and never copy a mnemonic into it. */
export function walletPreview(wallet: WalletSnapshot): WalletSnapshot {
  return {
    network: wallet.network, server: "", birthdayHeight: wallet.birthdayHeight,
    unifiedAddress: wallet.unifiedAddress, transparentAddress: wallet.transparentAddress,
    zip321: wallet.zip321, scannedHeight: wallet.scannedHeight, treesReady: wallet.treesReady,
    spendReady: false, viewOnly: wallet.viewOnly, ufvk: wallet.ufvk,
    ...(wallet.hardware ? { hardware: { ...wallet.hardware } } : {}),
    balance: { ...wallet.balance }, unlockPolicy: wallet.unlockPolicy,
  };
}

/** Materialize bytes only for hydration; Blob reads happen after the IDB transaction closes. */
export async function readSavedSnapshot(signal?: AbortSignal): Promise<Uint8Array | null> {
  return (await readSavedSnapshotRecord(signal))?.bytes ?? null;
}

/** Capture the durable wallet identity in the same read as the snapshot. */
export async function readSavedSnapshotRecord(signal?: AbortSignal): Promise<{ bytes: Uint8Array; generation: WalletGeneration; key?: string } | null> {
  if (!walletStorageAvailable()) return null;
  const saved = await readCurrentSnapshot<{ payload: SnapshotPayload | null; generation: WalletGeneration; key?: string }>((tx, value, generation) => {
    const ref = snapshotRef(value);
    if (generationValue(ref?.generation) !== generation) { tx.result({ payload: null, generation }); return; }
    if (!ref) { tx.result({ payload: snapshotPayload(value), generation }); return; }
    if (ref.format === INCREMENTAL_FORMAT) {
      readIncrementalSnapshot(tx, ref as ChunkIndex, bytes => tx.result({ payload: bytes, generation, key: ref.key }));
      return;
    }
    if (ref.format === CHUNKED_FORMAT) {
      const bytes = new Uint8Array(ref.byteLength);
      const count = Math.ceil(ref.byteLength / SNAPSHOT_CHUNK_BYTES);
      let remaining = count;
      for (let i = 0; i < count; i++) {
        tx.request(tx.store.get(chunkKey(ref.key, i)), value => {
          const chunk = snapshotBytes(value);
          const offset = i * SNAPSHOT_CHUNK_BYTES;
          if (!chunk || chunk.length !== Math.min(SNAPSHOT_CHUNK_BYTES, bytes.length - offset)) {
            throw new Error("Saved wallet snapshot is incomplete; keep the record and retry recovery");
          }
          bytes.set(chunk, offset);
          if (--remaining === 0) tx.result({ payload: bytes, generation, key: ref.key });
        });
      }
      return;
    }
    tx.request(tx.store.get(ref.key), value => {
      const stored = snapshotPayload(value);
      const length = isBlob(stored) ? stored.size : stored?.byteLength;
      tx.result({ payload: length === ref.byteLength ? stored : null, generation, key: ref.key });
    });
  }, signal);
  if (!saved.payload) return null;
  const payload = saved.payload;
  const bytes = isBlob(payload) ? new Uint8Array(await abortable(signal, () => payload.arrayBuffer())) : payload;
  if (signal?.aborted) throw signal.reason ?? aborted();
  return { bytes, generation: saved.generation, ...(saved.key ? { key: saved.key } : {}) };
}

/** The latest save's key (its revision) without reading the snapshot itself. */
export async function readSavedSnapshotKey(signal?: AbortSignal): Promise<string | null> {
  if (!walletStorageAvailable()) return null;
  return walletTransaction<string | null>("readonly", tx => {
    tx.request(tx.store.get(DEFAULT), value => tx.result(snapshotRef(value)?.key ?? null));
  }, signal);
}

/** Validate a skipped checkpoint without cloning the saved wallet payload. */
export async function assertSavedSnapshotCurrent(
  generation: WalletGeneration, key: string, signal?: AbortSignal,
): Promise<void> {
  await walletTransaction<void>("readonly", tx => {
    tx.request(tx.store.get(WALLET_GENERATION_KEY), value => {
      if (generationValue(value) !== generation) throw new WalletError("wallet_changed", "the saved wallet was replaced or forgotten");
      tx.request(tx.store.get(DEFAULT), value => {
        const ref = snapshotRef(value);
        if (!ref || ref.key !== key || generationValue(ref.generation) !== generation) {
          throw new WalletError("wallet_changed", "the wallet was saved by another tab");
        }
      });
    });
  }, signal);
}

/** Small record + key-existence read; legacy raw records are parsed only until the next save. */
export async function peekSavedSnapshot(
  parseLegacy: (bytes: Uint8Array) => WalletSnapshot | null,
): Promise<WalletSnapshot | null> {
  if (!walletStorageAvailable()) return null;
  return readCurrentSnapshot<WalletSnapshot | null>((tx, value, generation) => {
    tx.result(null);
    const ref = snapshotRef(value);
    if (generationValue(ref?.generation) !== generation) return;
    if (!ref) {
      const bytes = snapshotBytes(value);
      try { tx.result(bytes ? parseLegacy(bytes) : null); }
      catch { tx.result(null); }
      return;
    }
    tx.request(tx.store.getKey(ref.key), value => tx.result(value === ref.key ? ref.preview : null));
  });
}

/**
 * Commit the preview and its unique snapshot together, replacing the old pair
 * atomically. With `baseKey` (the save this state was loaded from or last
 * written as; null for none), the write is a compare-and-swap: if another tab
 * saved since, it fails with `wallet_changed` rather than overwrite that
 * tab's pending sends with an older state.
 */
export async function saveWalletSnapshot(
  bytes: Uint8Array, preview: WalletSnapshot, current: () => boolean,
  expectedGeneration?: WalletGeneration,
  baseKey?: string | null,
  /** A legacy record (no revision key) may be replaced. Spends must not set this. */
  replaceLegacy = false,
  /** Exact legacy bytes this save is allowed to replace. */
  legacyBytes?: Uint8Array | null,
  /** A restore's requested policy must commit with its first snapshot. */
  unlockPolicy?: UnlockPolicy,
  /** A restore's caller can cancel even after the last write request was queued. */
  signal?: AbortSignal,
  /** The initial replacement save must not be reported as failed after it committed. */
  commitWinsCancellation = false,
  /** Publish this prepared identity and snapshot in the same transaction. */
  replacement?: SnapshotReplacement,
): Promise<string | undefined> {
  if (!walletStorageAvailable()) return undefined;
  const controller = new AbortController();
  const abort = () => controller.abort(aborted("snapshot save cancelled"));
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  pendingWrites.add(abort);
  try {
    // Raw callers capture at entry. A hydrated session MUST pass its saved
    // identity rather than adopting the latest identity on each save.
    const generation = expectedGeneration ?? await readWalletGeneration(controller.signal);
    if (!current()) return undefined;
    const plan = bytes.byteLength > SNAPSHOT_CHUNK_BYTES ? await planSnapshotChunks(bytes, controller.signal) : undefined;
    let savedKey: string | undefined;
    await walletTransaction<void>("readwrite", tx => {
      const store = tx.store;
      const write = (previous?: SnapshotRef | null) => {
        const key = `${PREFIX}${crypto.randomUUID()}`;
        const ref: SnapshotRef = { format: plan ? INCREMENTAL_FORMAT : FORMAT, key, byteLength: bytes.byteLength,
          ...(plan ? { chunkCount: plan.length } : {}),
          preview: walletPreview(unlockPolicy ? { ...preview, unlockPolicy } : preview), generation };
        if (!snapshotRef(ref)) throw new Error("invalid snapshot preview");
        const commit = () => {
          if (!current()) throw aborted("snapshot save cancelled");
          store.put(ref, DEFAULT);
          if (replacement) store.put(generation, WALLET_GENERATION_KEY);
          if (unlockPolicy) store.put({ unlockPolicy }, "meta");
          savedKey = key;
        };
        // Migration/full replacement also clears orphaned legacy payloads.
        // Reuse is limited to the same wallet generation and CAS-checked revision.
        const reusable = plan && previous?.format === INCREMENTAL_FORMAT
          && generationValue(previous.generation) === generation ? previous as ChunkIndex : undefined;
        if (!reusable) store.deletePrefix(PREFIX);
        if (plan) writeIncrementalSnapshot(tx, bytes, plan, key, reusable, commit);
        else { store.put(bytes.slice(), key); commit(); }
      };
      tx.request(store.get(WALLET_GENERATION_KEY), value => {
        if (!current()) throw aborted("snapshot save cancelled");
        if (generationValue(value) !== (replacement?.previous ?? generation)) throw new WalletError("wallet_changed", "saved wallet changed; reload before saving");
        tx.request(store.get(DEFAULT), saved => {
          const ref = snapshotRef(saved);
          if (replacement) {
            const legacy = ref ? null : snapshotPayload(saved);
            const expected = replacement.previousLegacy;
            const sameLegacy = legacy == null && expected == null || !!legacy && !!expected &&
              (isBlob(legacy) && isBlob(expected) ? legacy.size === expected.size :
                !isBlob(legacy) && !isBlob(expected) && sameBytes(legacy, expected));
            if ((ref?.key ?? null) !== replacement.previousKey || !sameLegacy) {
              throw new WalletError("wallet_changed", "the wallet was saved by another tab");
            }
            write(ref);
            return;
          }
          if (baseKey === undefined) { write(ref); return; }
          // A raw legacy record has no revision. `null === null` must not
          // treat it as "nothing saved", or a stale tab overwrites it.
          if (saved != null && !ref) {
            // Legacy blobs are not readable synchronously inside this transaction.
            // Their length is the comparison available before the replacement write.
            const raw = snapshotPayload(saved);
            const sameLegacy = !!raw && !!legacyBytes && (isBlob(raw)
              ? raw.size === legacyBytes.byteLength
              : sameBytes(raw, legacyBytes));
            if (!replaceLegacy || !sameLegacy) {
              throw new WalletError("wallet_changed", "the wallet was saved by another tab");
            }
            write();
            return;
          }
          // Another wallet generation's leftover is no save of this wallet.
          const stored = ref && generationValue(ref.generation) === generation ? ref.key : null;
          if (stored !== baseKey) throw new WalletError("wallet_changed", "the wallet was saved by another tab");
          write(ref);
        });
      });
    }, controller.signal, commitWinsCancellation);
    // Each save has its own key: callers use it as the save's revision.
    return savedKey;
  } catch (error) {
    if (current()) {
      if (error && typeof error === "object" && "name" in error && error.name === "QuotaExceededError") {
        throw WalletError.fromUnknown(error);
      }
      throw error;
    }
    return undefined;
  } finally {
    signal?.removeEventListener("abort", abort);
    pendingWrites.delete(abort);
  }
}

export async function clearSavedSnapshot(): Promise<void> {
  if (!walletStorageAvailable()) return;
  await walletTransaction<void>("readwrite", tx => {
    deleteSnapshotRecords(tx.store);
    tx.store.put(crypto.randomUUID(), WALLET_GENERATION_KEY);
  });
}

function deleteSnapshotRecords(store: WalletRecordStore): void {
  store.deletePrefix(PREFIX);
  store.delete(DEFAULT);
  store.delete("meta");
}

/** Forget is all-or-nothing and leaves a tombstone fencing other tabs' old sessions. */
export async function clearSavedWallet(opts?: { passkey?: boolean }): Promise<void> {
  if (!walletStorageAvailable()) throw new WalletError("wallet_db", "Local storage is unavailable. This device’s saved wallet has not been deleted.");
  invalidateVaultOperations();
  await walletTransaction<void>("readwrite", tx => {
    deleteSnapshotRecords(tx.store);
    tx.store.delete("seed.enc");
    if (opts?.passkey) {
      tx.store.delete("passkey.v1");
      tx.store.delete("passkey.vault.v1");
    }
    tx.store.put(crypto.randomUUID(), WALLET_GENERATION_KEY);
  });
}
