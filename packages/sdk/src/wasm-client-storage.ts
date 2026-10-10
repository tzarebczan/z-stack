import {
  isWalletError,
  WalletError,
  type UnlockPolicy,
  type WalletSnapshot
} from "@z-stack/core";
import {
  canUseScanWorker,
  prepareScanWorkerForNewWallet,
  recoverScanWorker,
  restartScanWorker,
  scanWorkerFailed,
  scanWorkerReady,
  scanWorkerStarting,
  treeConflictUserMessage,
  workerScanSession,
  type ScanSession
} from "./scan-host";
import {
  assertSavedSnapshotCurrent,
  saveWalletSnapshot as idbPut,
  readSavedSnapshotKey,
  readSavedSnapshotRecord,
  prepareSnapshotReplacement,
} from "./snapshot-storage";
import { captureWalletOperation, type WalletOperation } from "./wallet-lifecycle";
import { readWalletGeneration, walletStorageAvailable, type WalletGeneration } from "./wallet-storage";
import type { WasmClientContext } from './wasm-client-context';
import * as runtime from './wasm-client-runtime';

/** Snapshot durability, hydration and wallet replacement. */
export function createStorageController(ctx: Pick<WasmClientContext, "savedRevision" | "savedKeySeen" | "legacyBytes" | "replaceLegacyOnSave" | "storageGeneration" | "session" | "persistenceRequired" | "sessionOperation" | "walletEpoch" | "stateEpoch" | "assertSource" | "unlockPolicy" | "spendingSeed" | "network" | "currentSession" | "loadLock" | "report" | "publishSession" | "paintNoteBalance" | "bindLocal" | "retireFailedSession" | "opts">) {

  function copyLegacy(record: { bytes: Uint8Array; key?: string } | null | undefined): Uint8Array | null {
    if (!record || record.key != null || record.bytes.byteLength === 0) return null;
    return record.bytes.slice();
  }

  function applyLoaded(record: { key?: string } | null | undefined, legacy: Uint8Array | null): void {
    // Hydration can migrate/reconcile Rust state. Save it once before reusing it.
    ctx.savedRevision = undefined;
    ctx.savedKeySeen = record?.key ?? null;
    ctx.legacyBytes = legacy;
    ctx.replaceLegacyOnSave = legacy != null;
  }

  /**
   * `lineage` pins the revision and epoch a spend proved against. The save
   * must not use a revision adopted from another tab, or the proved bytes
   * would be written on top of that tab's snapshot.
   */
  function persist(
    lineage?: { epoch: number; baseKey: string | null },
    replacement?: runtime.ClaimedGeneration & { unlockPolicy?: UnlockPolicy },
    background = false,
  ): Promise<void> {
    // A background/pagehide save must not publish the new snapshot before its
    // requested policy, nor replace this mandatory save in the coalescing queue.
    if (ctx.storageGeneration && runtime.pendingReplacementSaves.has(ctx.storageGeneration) && ctx.storageGeneration !== replacement?.generation) {
      return background ? Promise.resolve() : Promise.reject(new WalletError("wallet_changed", "wallet restore is still in progress"));
    }
    if (!ctx.session || replacement && replacement.generation !== ctx.storageGeneration) {
      return replacement || lineage ? Promise.reject(new DOMException("mandatory snapshot save cancelled", "AbortError")) : Promise.resolve();
    }
    if (!walletStorageAvailable()) return background || !ctx.persistenceRequired ? Promise.resolve()
      : Promise.reject(new WalletError("wallet_db", "Local wallet storage is unavailable. Nothing was saved."));
    const source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    const storedGeneration = ctx.storageGeneration;
    const epoch = ctx.walletEpoch;
    const generation = runtime.runtimeState.persistGen;
    const ownsSession = () => !operation.signal.aborted && source === ctx.session && epoch === ctx.walletEpoch && generation === runtime.runtimeState.persistGen
      && (source.kind !== "worker" || source === workerScanSession());
    const current = () => {
      if (!ownsSession()) return false;
      operation.assertCurrent();
      return true;
    };
    const canSave = () => {
      if (current()) return true;
      if (replacement || lineage) throw new DOMException("mandatory snapshot save cancelled", "AbortError");
      return false;
    };
    const run = async () => {
      const baseKey = lineage ? lineage.baseKey : ctx.savedKeySeen;
      const allowLegacy = !lineage && ctx.replaceLegacyOnSave;
      const legacy = allowLegacy ? ctx.legacyBytes : null;
      if (!canSave()) return;
      if (ctx.persistenceRequired && !background && !walletStorageAvailable()) {
        throw new WalletError("wallet_db", "Local wallet storage is unavailable. Nothing was saved.");
      }
      if (lineage && ctx.stateEpoch !== lineage.epoch) {
        throw new WalletError("wallet_changed", "the wallet was saved by another tab");
      }
      try {
        const revision = source.persistenceRevision?.();
        if (!lineage && !replacement && storedGeneration && revision !== undefined
          && ctx.savedRevision?.source === source && ctx.savedRevision.revision === revision
          && ctx.savedRevision.key === baseKey) {
          // Even a no-op must observe another tab's save or Forget. Check both
          // identity and revision atomically before acknowledging durability.
          await assertSavedSnapshotCurrent(storedGeneration, ctx.savedRevision.key, operation.signal);
          if (!canSave()) return;
          if (ctx.persistenceRequired && !background && !walletStorageAvailable()) {
            throw new WalletError("wallet_db", "Local wallet storage is unavailable. Nothing was saved.");
          }
          if (source.persistenceRevision?.() === revision) return;
        }
        const captureRevision = source.persistenceRevision?.();
        const { bytes, previewJson } = await source.persistenceSnapshot();
        const stable = captureRevision !== undefined && captureRevision === source.persistenceRevision?.();
        // Recheck after worker serialization AND after opening IndexedDB. Wipe,
        // forget and replacement invalidate even pagehide/explicit saves.
        if (!canSave()) return;
        if (lineage && ctx.stateEpoch !== lineage.epoch) {
          throw new WalletError("wallet_changed", "the wallet was saved by another tab");
        }
        const key = await idbPut(bytes, JSON.parse(previewJson) as WalletSnapshot, current, storedGeneration, baseKey, allowLegacy, legacy, replacement?.unlockPolicy, operation.signal, !!replacement, replacement);
        // Background saves are best effort. Replacement and spend/rollback
        // checkpoints must distinguish a durable commit from a cancelled no-op.
        if (!key && ctx.persistenceRequired && !background && ownsSession()) {
          throw new WalletError("wallet_db", "Local wallet storage is unavailable. Nothing was saved.");
        }
        if ((replacement || lineage) && !key) throw new DOMException("mandatory snapshot save cancelled", "AbortError");
        // A durable key is the success boundary. Never invoke caller guards
        // after it: throwing here would roll back only the generation while
        // the replacement snapshot is already committed.
        if (key && ownsSession()) {
          ctx.savedKeySeen = key;
          ctx.savedRevision = stable ? { source, revision: captureRevision!, key } : undefined;
          if (!lineage) {
            ctx.replaceLegacyOnSave = false;
            ctx.legacyBytes = null;
          }
        }
      } catch (e) {
        if (!isWalletError(e) || e.code !== "wallet_changed" || !current()) throw e;
        // Another tab saved since this one loaded. Take its state (its pending
        // sends and spent notes) instead of overwriting it; this tab's unsaved
        // scan progress is redone, and the caller's operation fails.
        await adoptSaved(source, operation);
        throw e;
      }
    };
    // A spend or rollback save must not be replaced by a later background save.
    if (lineage || replacement) return run();
    return runtime.saveSnapshot(run);
  }

  /** Replace this tab's copy with the saved wallet. Fails if the wallet itself changed. */
  async function adoptSaved(source: ScanSession, operation: WalletOperation): Promise<void> {
    const record = await readSavedSnapshotRecord(operation.signal);
    ctx.assertSource(operation, source);
    if (!record || record.generation !== ctx.storageGeneration) {
      throw new WalletError("wallet_changed", "the saved wallet was replaced or forgotten in another tab; reload it");
    }
    const legacy = copyLegacy(record);
    await source.fromSnapshot(record.bytes);
    ctx.assertSource(operation, source);
    applyLoaded(record, legacy);
    ctx.stateEpoch += 1;
  }

  /**
   * Under the spend lock: if another tab saved the wallet since this tab last
   * loaded or saved it, spend from that newer state (its pending sends and
   * spent notes), not from this tab's stale copy. An unknown revision (a
   * legacy record, a failed read) never counts as fresh.
   */
  async function refreshIfStale(source: ScanSession, operation: WalletOperation): Promise<void> {
    if (!walletStorageAvailable()) return;
    const latest = await readSavedSnapshotKey(operation.signal);
    // `null === null` is not fresh: a legacy record has no key, and neither
    // does "nothing saved". Only a verified empty database skips the reload.
    if (ctx.savedKeySeen !== null && latest === ctx.savedKeySeen) return;
    if (ctx.savedKeySeen === null && latest === null) {
      const record = await readSavedSnapshotRecord(operation.signal);
      if (!record) return;
    }
    await adoptSaved(source, operation);
    // A legacy raw record has no revision key. Adopting it is not another tab.
    if (ctx.savedKeySeen === null && !ctx.replaceLegacyOnSave) {
      throw new WalletError("wallet_changed", "the wallet was saved by another tab");
    }
  }

  function adoptSessionSeed(): void {
    if (ctx.unlockPolicy === "each-spend") return;
    const stored = runtime.loadSessionSeed();
    if (!stored) return;
    ctx.spendingSeed = stored;
    runtime.runtimeState.moduleSpendingSeed = stored;
  }

  function peekedNetwork(raw: string | undefined): runtime.Network {
    return raw === "mainnet" || raw === "testnet" || raw === "regtest" ? raw : ctx.network;
  }

  async function loadIfNeeded(): Promise<boolean> {
    if (ctx.currentSession()) {
      adoptSessionSeed();
      return true;
    }
    if (ctx.loadLock) return ctx.loadLock;
    const pending = loadIfNeededInner();
    ctx.loadLock = pending;
    const release = () => { if (ctx.loadLock === pending) ctx.loadLock = null; };
    void pending.then(release, release);
    return pending;
  }

  async function loadIfNeededInner(): Promise<boolean> {
    if (ctx.session) {
      adoptSessionSeed();
      return true;
    }
    const operation = captureWalletOperation();
    // Capture before awaiting storage: a failure during this request must reject
    // this request, rather than silently replaying it on a newly spawned worker.
    const recovering = scanWorkerFailed();
    await operation.ready();
    const epoch = ctx.walletEpoch;
    const generation = runtime.runtimeState.persistGen;
    const invalidated = () => operation.signal.aborted || epoch !== ctx.walletEpoch || generation !== runtime.runtimeState.persistGen;
    const record = await runtime.withTimeout(readSavedSnapshotRecord(operation.signal), runtime.IDB_READ_MS, "IndexedDB wallet read timed out")
      .catch((error) => { if (operation.signal.aborted) return null; throw error; });
    const bytes = record?.bytes;
    if (ctx.session || invalidated()) {
      if (ctx.session) adoptSessionSeed();
      return !!ctx.session;
    }
    if (!bytes) return false;
    const peeked = runtime.peekSnapshotBytes(bytes);
    ctx.report({
      stage: "connecting",
      heading: "Restoring snapshot",
      activity: "loading",
      message:
        peeked && (peeked.balance.totalAvailable ?? 0) > 0
          ? "available balance from decrypted notes"
          : `hydrating ${Math.max(1, Math.round(bytes.byteLength / 1024))} KB in the scan worker`,
      scanned: peeked?.scannedHeight ?? 0,
      tip: 0,
      notesFound: 0,
      spendsFound: 0,
      percent: 8,
      paintWallet: (peeked?.balance.totalAvailable ?? 0) > 0,
      availableZat: peeked?.balance.totalAvailable,
      pendingZat: peeked?.balance.totalPending ?? 0,
      availableZec: peeked?.balance.totalZec,
      pendingZec: peeked?.balance.pendingZec,
      orchardAvailable: peeked?.balance.orchardAvailable,
    });
    if (invalidated()) return !!ctx.session;
    if (recovering && !scanWorkerReady()) {
      await recoverScanWorker();
    } else if (!scanWorkerReady()) {
      try {
        await runtime.withTimeout(scanWorkerStarting(), runtime.WORKER_START_MS, "scan worker did not start");
      } catch {
        if (invalidated()) return false;
        console.warn("scan worker start");
      }
    }
    if (invalidated()) return !!ctx.session;
    const worker = workerScanSession();
    if (worker) {
      try {
        if (runtime.runtimeState.liveWorkerWallet?.session === worker && runtime.runtimeState.liveWorkerWallet.operation.signal === operation.signal
          && runtime.runtimeState.liveWorkerWallet.generation !== record!.generation) {
          throw new Error("wallet changed in another tab; reload before continuing");
        }
        const legacy = copyLegacy(record);
        if (!runtime.runtimeState.liveWorkerWallet || runtime.runtimeState.liveWorkerWallet.session !== worker || runtime.runtimeState.liveWorkerWallet.operation.signal !== operation.signal) {
          if (!runtime.runtimeState.workerHydration || runtime.runtimeState.workerHydration.session !== worker || runtime.runtimeState.workerHydration.signal !== operation.signal) {
            const promise = worker.fromSnapshot(bytes);
            runtime.runtimeState.workerHydration = { session: worker, signal: operation.signal, promise };
          }
          const pending = runtime.runtimeState.workerHydration;
          try { await pending.promise; }
          finally { if (runtime.runtimeState.workerHydration === pending) runtime.runtimeState.workerHydration = null; }
          if (invalidated()) return false;
          runtime.runtimeState.liveWorkerWallet = { session: worker, operation, generation: record!.generation };
        }
        if (invalidated()) return false;
        ctx.publishSession(worker, operation, runtime.runtimeState.liveWorkerWallet.generation);
        applyLoaded(record, legacy);
        adoptSessionSeed();
        const got = await worker.scannedHeight();
        const peekH = peeked?.scannedHeight ?? 0;
        if (peekH > 0 && got + 32 < peekH) {
          throw new Error(
            `snapshot height ${peekH} did not survive hydrate (now ${got})`,
          );
        }
        await ctx.paintNoteBalance({
          stage: "connecting",
          heading: "Restoring snapshot",
          activity: "loading",
          message: "available balance from decrypted notes",
          scanned: got,
          tip: 0,
          notesFound: 0,
          spendsFound: 0,
          percent: 10,
        });
        return true;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (invalidated()) return false;
        console.warn("scan worker fromSnapshot failed");
        throw new Error(treeConflictUserMessage(`could not restore snapshot: ${msg}`));
      }
    }
    if (canUseScanWorker()) {
      throw new Error(
        "scan worker failed to start; snapshot was not restored on the page thread (that would freeze the tab)",
      );
    }
    const legacy = copyLegacy(record);
    ctx.bindLocal(runtime.requireBindings().WasmWallet.fromSnapshot(bytes), operation, record!.generation);
    applyLoaded(record, legacy);
    adoptSessionSeed();
    const got = await ctx.session!.scannedHeight();
    const peekH = peeked?.scannedHeight ?? 0;
    if (peekH > 0 && got + 32 < peekH) {
      throw new Error(`snapshot height ${peekH} did not survive hydrate (now ${got})`);
    }
    await ctx.paintNoteBalance({
      stage: "connecting",
      heading: "Restoring snapshot",
      activity: "loading",
      message: "available balance from decrypted notes",
      scanned: got,
      tip: 0,
      notesFound: 0,
      spendsFound: 0,
      percent: 10,
    });
    return true;
  }

  async function preferWorker(): Promise<void> {
    const worker = workerScanSession();
    if (!worker || !ctx.session || ctx.session === worker) return;
    const previous = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    try {
      const bytes = await previous.toSnapshot();
      ctx.assertSource(operation, previous);
      await worker.fromSnapshot(bytes);
      ctx.assertSource(operation, previous);
      ctx.publishSession(worker, operation, ctx.storageGeneration!);
    } catch {
      operation.assertCurrent();
      console.warn("could not move wallet onto scan worker");
    }
  }

  async function persistReplacement(claimed: runtime.ClaimedGeneration, policy?: UnlockPolicy): Promise<void> {
    await persist(undefined, { ...claimed, unlockPolicy: policy });
    runtime.pendingReplacementSaves.delete(claimed.generation);
    claimed.complete();
  }

  /** Retire an unsaved replacement; the previous durable wallet never changed. */
  async function releaseReplacement(operation: WalletOperation, claimed: runtime.ClaimedGeneration): Promise<void> {
    if (ctx.storageGeneration === claimed.generation) {
      if (ctx.session) ctx.retireFailedSession(ctx.session);
      ctx.storageGeneration = claimed.previous;
      ctx.spendingSeed = null;
      if (!operation.signal.aborted) {
        runtime.runtimeState.moduleSpendingSeed = null;
        runtime.saveSessionSeed(null);
      }
    }
    runtime.pendingReplacementSaves.delete(claimed.generation);
    claimed.complete();
  }

  async function replaceWallet(
    operation: WalletOperation,
    viaWorker: (s: ScanSession) => Promise<void>,
    makeLocal: () => runtime.WasmWalletHandle,
  ): Promise<runtime.ClaimedGeneration> {
    let resolve!: () => void;
    const completion = new Promise<void>(done => { resolve = done; });
    runtime.pendingReplacementCompletions.add(completion);
    const complete = () => { runtime.pendingReplacementCompletions.delete(completion); resolve(); };
    let claimed: runtime.ClaimedGeneration | undefined;
    try {
      await operation.ready();
      const previous = await (runtime.replacementBases.get(operation.signal)
        ?? (ctx.storageGeneration ? Promise.resolve(ctx.storageGeneration) : readWalletGeneration(operation.signal)));
      const replacement = await prepareSnapshotReplacement(operation.signal, previous,
        !ctx.opts.requireExplicitReplacement || runtime.replacementConsent.get(operation.signal) === true);
      claimed = { ...replacement, complete };
      const generation = claimed.generation;
      operation.assertCurrent();
      runtime.pendingReplacementSaves.add(generation);
      if (canUseScanWorker() && !scanWorkerReady()) {
        await restartScanWorker(runtime.spawnScanWorker);
        operation.assertCurrent();
      }
      await prepareScanWorkerForNewWallet(runtime.spawnScanWorker);
      operation.assertCurrent();
      const worker = workerScanSession();
      if (worker) {
        try {
          await viaWorker(worker);
          operation.assertCurrent();
          ctx.publishSession(worker, operation, generation);
          return claimed;
        } catch (e) {
          operation.assertCurrent();
          if (worker !== workerScanSession()) throw e;
          console.warn("scan worker wallet create failed; deriving on this thread");
          await restartScanWorker(runtime.spawnScanWorker);
          operation.assertCurrent();
        }
      }
      const handle = makeLocal();
      let adopted = false;
      try {
        operation.assertCurrent();
        const moved = workerScanSession();
        if (moved) {
          try {
            await moved.fromSnapshot(handle.toSnapshot());
            operation.assertCurrent();
            ctx.publishSession(moved, operation, generation);
            return claimed;
          } catch (e) {
            operation.assertCurrent();
            if (moved !== workerScanSession()) throw e;
            console.warn("could not move wallet onto scan worker");
          }
        }
        ctx.bindLocal(handle, operation, generation);
        adopted = true;
      } finally { if (!adopted) handle.free?.(); }
      return claimed;
    } catch (e) {
      if (claimed) await releaseReplacement(operation, claimed);
      else complete();
      throw e;
    }
  }
  return { copyLegacy, applyLoaded, persist, adoptSaved, refreshIfStale, peekedNetwork, loadIfNeeded, preferWorker, persistReplacement, releaseReplacement, replaceWallet };
}
