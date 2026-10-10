import {
  filterHistory,
  parseZip321,
  WalletError,
  type HistoryEntry,
  type HistoryQuery,
  type UnlockPolicy,
  type WalletSnapshot
} from "@z-stack/core";
import type { ChainTip, CreationResult, EngineHealth, WaitOpts } from "./engine";
import { balanceEvent, createEventBus } from "./events";
import { observeWasmRuntime, wasmRuntime } from "./runtime";
import type { WalletEventHandler } from "./events";
import {
  type HardwareAccount
} from "./hardware";
import { usesFastSync } from "./lwd";
import { type PublicDataStatus } from "./public-data";
import {
  forgetScanWorkerWallet,
  localScanSession,
  workerScanSession,
  type ScanSession
} from "./scan-host";
import {
  readSavedSnapshot as idbGet,
  readSavedSnapshotRecord,
  readSavedSnapshotKey
} from "./snapshot-storage";
import { beginWalletOperation, cancelWalletOperation, captureWalletOperation, type WalletOperation } from "./wallet-lifecycle";
import { abortable, readWalletGeneration, walletStorageAvailable, type WalletGeneration } from "./wallet-storage";
import type { WasmClientContext } from './wasm-client-context';
import * as runtime from './wasm-client-runtime';
import { createSpendingController } from './wasm-client-spending';
import { createStorageController } from './wasm-client-storage';
import { createSyncController } from './wasm-client-sync';
export { attachWasmBindings, cancelWasmSync, configureRegtestNu63Height, configureRegtestNu7Height, configureWasmWorkerBasePath, forgetWasmWallet, isDuplicateBroadcastError, orchardProvingKeyReady, peekSnapshotBytes, peekWasmWallet, prewarmOrchardProvingKey, prewarmProveWorker, reorgRestartFrom, setWasmSpendingSeed, startScanWorker, submitVerdict, wasmCapabilities } from './wasm-client-runtime';
export type { WasmClient, WasmClientOpts, WasmProgress } from './wasm-client-runtime';
export function createWasmClient(
  opts: runtime.WasmClientOpts,
  onProgress?: (p: runtime.WasmProgress) => void,
): runtime.WasmClient {

  const { network, transport } = opts;

  const persistenceRequired = opts.requirePersistence === true || walletStorageAvailable();

  const bus = createEventBus();
  const offRuntime = observeWasmRuntime(current => bus.emit("runtime", current));

  let session: ScanSession | null = null;

  let sessionOperation: WalletOperation | null = null;

  let rootsChecked: { source: ScanSession; at: number } | null = null;

  let storageGeneration: WalletGeneration | undefined;

  let loadLock: Promise<boolean> | null = null;

  let walletEpoch = 0;

  let spendingSeed: string | null = null;

  let disposing = false;
  let rescanning = false;

  // Interrupt external device prompts before draining reservations. Aborting the
  // wallet operation itself here would destroy the session needed for rollback.
  const disposeWaits = new AbortController();

  let reservationDrain: Promise<void> | undefined;

  function beginReservationDrain() {
    let resolve!: () => void;
    let reject!: (error: unknown) => void;
    reservationDrain = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
    // Dispose observes failure when it is requested; no unhandled rejection otherwise.
    void reservationDrain.catch(() => { });
    return (error?: unknown) => error === undefined ? resolve() : reject(error);
  }

  /** Key (revision) of the saved snapshot this tab last loaded or wrote. */
  let savedKeySeen: string | null = null;

  /** Only a successfully committed serialization can satisfy later checkpoints. */
  let savedRevision: { source: ScanSession; revision: number; key: string } | undefined;

  /** True only while the loaded record is a legacy blob with no revision key. */
  let replaceLegacyOnSave = false;

  /** Exact legacy bytes this tab loaded. Copied before the worker detaches the buffer. */
  let legacyBytes: Uint8Array | null = null;

  /** Bumps when this tab replaces its in-memory wallet with another tab's save. */
  let stateEpoch = 0;

  let latestOperation = captureWalletOperation();

  let unlockPolicy: UnlockPolicy = opts.unlockPolicy ?? "each-spend";

  let memoFetch = opts.memoFetch ?? "on-demand";

  let prewarmProvingKey = opts.prewarmProvingKey !== false;

  let selectiveMemoStatus: PublicDataStatus = memoFetch === "on-demand" ? "off" : "scanning";

  let memoAbort: AbortController | null = null;

  let transparentScan = opts.transparentScan ?? "off";

  let transparentScanStatus: PublicDataStatus = transparentScan === "compact" ? "scanning" : "off";

  let sharedMemoStatus: PublicDataStatus = memoFetch === "shared" ? "scanning" : "off";

  const serverLabel = transport.label;

  const localLight = usesFastSync(opts.lightUrl ?? transport.label);

  const grpcWeb = transport.kind === "grpc-web";

  const lwdPipe = transport.kind === "lwd-pipe";

  const syncOpts = { grpcWeb, lwdPipe };

  // Unlock policy belongs to the application, never to untrusted saved data.
  runtime.saveSessionSeed(null);

  function publishSession(next: ScanSession, operation: WalletOperation, generation: WalletGeneration): void {
    operation.assertCurrent();
    if (next.kind === "worker" && next !== workerScanSession()) throw new Error("scan worker restarted");
    const previous = session;
    session = next;
    rootsChecked = null;
    sessionOperation = operation;
    // A new wallet (generation) has no save yet; loads set the key they read.
    if (generation !== storageGeneration) savedKeySeen = null;
    storageGeneration = generation;
    if (next.kind === "worker") runtime.runtimeState.liveWorkerWallet = { session: next, operation, generation };
    if (previous !== next && previous?.kind === "local") void previous.forget();
    operation.signal.addEventListener("abort", () => {
      if (session !== next || sessionOperation?.signal !== operation.signal) return;
      walletEpoch++;
      spendingSeed = null;
      session = null;
      sessionOperation = null;
      if (next.kind === "local") void next.forget();
    }, { once: true });
  }

  function bindLocal(handle: runtime.WasmWalletHandle, operation: WalletOperation, generation: WalletGeneration): ScanSession {
    const next = localScanSession(
      handle,
      (bytes) => runtime.requireBindings().WasmWallet.fromSnapshot(bytes),
      runtime.requireBindings().WasmWallet,
    );
    publishSession(next, operation, generation);
    for (const ref of runtime.localWalletSessions) if (!ref.deref()) runtime.localWalletSessions.delete(ref);
    runtime.localWalletSessions.add(new WeakRef(next));
    return next;
  }

  function startReplacement(assertCurrent?: () => void): WalletOperation {
    const predecessors = [...runtime.pendingReplacementCompletions];
    const base = beginWalletOperation();
    const operation: WalletOperation = assertCurrent ? {
      signal: base.signal,
      assertCurrent() { base.assertCurrent(); assertCurrent(); },
      assertReady() { base.assertReady(); assertCurrent(); },
      async ready() { await base.ready(); assertCurrent(); },
    } : base;
    latestOperation = operation;
    runtime.cancelWasmSync();
    runtime.runtimeState.moduleSpendingSeed = null;
    runtime.saveSessionSeed(null);
    // A superseded initial commit can still be finishing its cleanup.
    // Capture the successor's baseline only after that cleanup settles.
    const baseline = operation.ready().then(() => Promise.all(predecessors)).then(() => readWalletGeneration(operation.signal));
    void baseline.catch(() => { });
    runtime.replacementBases.set(operation.signal, baseline);
    return operation;
  }

  function assertSource(operation: WalletOperation, source: ScanSession): void {
    operation.assertCurrent();
    if (source !== session) throw new DOMException("wallet operation cancelled", "AbortError");
    if (source.kind === "worker" && source !== workerScanSession()) throw new Error("scan worker restarted");
  }

  /**
   * A failed block apply leaves the engine refusing every later apply and save
   * for this wallet instance. Drop it so the next sync hydrates the last saved
   * snapshot again, instead of every background sync failing until a reload.
   */
  function retireFailedSession(stale: ScanSession): void {
    if (session !== stale) return;
    walletEpoch++;
    session = null;
    sessionOperation = null;
    if (runtime.runtimeState.liveWorkerWallet?.session === stale) runtime.runtimeState.liveWorkerWallet = null;
    if (runtime.runtimeState.workerHydration?.session === stale) runtime.runtimeState.workerHydration = null;
    if (stale.kind === "local") void stale.forget();
  }

  /** A dead worker lease must not keep bypassing explicit snapshot hydration. */
  function currentSession(): ScanSession | null {
    const active = workerScanSession();
    const invalidate = (stale: ScanSession) => {
      // Several clients can own this same failed lease. Cancel its old work once,
      // so later callers do not invalidate the first caller's shared recovery.
      if (!runtime.retiredWorkerSessions.has(stale)) {
        runtime.retiredWorkerSessions.add(stale);
        runtime.cancelWasmSync();
      }
      if (runtime.runtimeState.liveWorkerWallet?.session === stale) runtime.runtimeState.liveWorkerWallet = null;
      if (runtime.runtimeState.workerHydration?.session === stale) runtime.runtimeState.workerHydration = null;
    };
    if (runtime.runtimeState.liveWorkerWallet && runtime.runtimeState.liveWorkerWallet.session !== active) invalidate(runtime.runtimeState.liveWorkerWallet.session);
    if (session?.kind === "worker" && session !== active) {
      const stale = session;
      walletEpoch++;
      session = null;
      sessionOperation = null;
      invalidate(stale);
    }
    return session;
  }

  async function snap(): Promise<WalletSnapshot> {
    if (!session) throw new Error("no wasm wallet");
    const source = session;
    const operation = sessionOperation ?? captureWalletOperation();
    const w = JSON.parse(await source.snapshotJson(serverLabel)) as WalletSnapshot;
    assertSource(operation, source);
    // Completion describes the current local queue, including transactions found
    // by a later sync. Inspecting it sends no transaction IDs to the server.
    if (memoFetch === "on-demand" && selectiveMemoStatus === "complete") {
      const pending = await source.memoEnhancementTxids(1);
      assertSource(operation, source);
      if (memoFetch === "on-demand" && selectiveMemoStatus === "complete" && pending?.length) {
        selectiveMemoStatus = "scanning";
      }
    }
    return decorateSnapshot(w);
  }

  function decorateSnapshot(w: WalletSnapshot): WalletSnapshot {
    if (w.hardware == null) delete w.hardware;
    w.unlockPolicy = unlockPolicy;
    w.transparentScanStatus = transparentScanStatus;
    w.sharedMemoStatus = memoFetch === "shared" ? sharedMemoStatus : "off";
    w.memoFetchStatus = memoFetch === "shared" ? sharedMemoStatus : selectiveMemoStatus;
    return w;
  }

  runtime.bindPersistOnHide();

  runtime.runtimeState.persistOnHide = () => rescanning ? Promise.resolve() : persist(undefined, undefined, true);

  let syncLock: { epoch: number; promise: Promise<WalletSnapshot> } | null = null;

  let memoLock: { epoch: number; promise: Promise<WalletSnapshot> } | null = null;

  async function loadHistory(limit = 50, query?: HistoryQuery): Promise<HistoryEntry[]> {
    if (!currentSession() && !(await loadIfNeeded())) return [];
    const filtered = !!(query?.status || query?.txid?.trim());
    const fetchLimit = filtered ? 500 : limit;
    const fromWallet = JSON.parse(await session!.history(fetchLimit)) as HistoryEntry[];
    const rows = fromWallet.length ? fromWallet : ((await snap()).transactions ?? []);
    return filterHistory(rows, query, limit);
  }

  async function updateUnlockPolicy(policy: UnlockPolicy, operation: WalletOperation, expected: WalletGeneration): Promise<void> {
    assertReplacementSaved(expected);
    operation.assertCurrent();
    applyUnlockPolicy(policy);
  }

  function applyUnlockPolicy(policy: UnlockPolicy): void {
    unlockPolicy = policy;
    if (policy === "each-spend") {
      spendingSeed = null;
      runtime.runtimeState.moduleSpendingSeed = null;
      runtime.saveSessionSeed(null);
    }
  }

  function assertReplacementSaved(generation = storageGeneration): void {
    if (rescanning) throw new WalletError("busy", "rescan is in progress");
    if (generation && runtime.pendingReplacementSaves.has(generation)) {
      throw new WalletError("wallet_changed", "wallet restore is still in progress");
    }
  }

  // Factories declare operations without executing them; getters resolve the
  // current owner state and peer capabilities when an operation starts.
  const ctx: WasmClientContext = {
    get savedRevision() { return savedRevision; },
    set savedRevision(value: WasmClientContext["savedRevision"]) { savedRevision = value; },
    get savedKeySeen() { return savedKeySeen; },
    set savedKeySeen(value: WasmClientContext["savedKeySeen"]) { savedKeySeen = value; },
    get legacyBytes() { return legacyBytes; },
    set legacyBytes(value: WasmClientContext["legacyBytes"]) { legacyBytes = value; },
    get replaceLegacyOnSave() { return replaceLegacyOnSave; },
    set replaceLegacyOnSave(value: WasmClientContext["replaceLegacyOnSave"]) { replaceLegacyOnSave = value; },
    get storageGeneration() { return storageGeneration; },
    set storageGeneration(value: WasmClientContext["storageGeneration"]) { storageGeneration = value; },
    get session() { return session; },
    set session(value: WasmClientContext["session"]) { session = value; },
    get persistenceRequired() { return persistenceRequired; },
    get sessionOperation() { return sessionOperation; },
    set sessionOperation(value: WasmClientContext["sessionOperation"]) { sessionOperation = value; },
    get walletEpoch() { return walletEpoch; },
    set walletEpoch(value: WasmClientContext["walletEpoch"]) { walletEpoch = value; },
    get stateEpoch() { return stateEpoch; },
    set stateEpoch(value: WasmClientContext["stateEpoch"]) { stateEpoch = value; },
    get assertSource() { return assertSource; },
    get unlockPolicy() { return unlockPolicy; },
    set unlockPolicy(value: WasmClientContext["unlockPolicy"]) { unlockPolicy = value; },
    get spendingSeed() { return spendingSeed; },
    set spendingSeed(value: WasmClientContext["spendingSeed"]) { spendingSeed = value; },
    get network() { return network; },
    get currentSession() { return currentSession; },
    get loadLock() { return loadLock; },
    set loadLock(value: WasmClientContext["loadLock"]) { loadLock = value; },
    get report() { return report; },
    get publishSession() { return publishSession; },
    get paintNoteBalance() { return paintNoteBalance; },
    get bindLocal() { return bindLocal; },
    get retireFailedSession() { return retireFailedSession; },
    get opts() { return opts; },
    get onProgress() { return onProgress; },
    get bus() { return bus; },
    get snap() { return snap; },
    get syncLock() { return syncLock; },
    set syncLock(value: WasmClientContext["syncLock"]) { syncLock = value; },
    get memoLock() { return memoLock; },
    set memoLock(value: WasmClientContext["memoLock"]) { memoLock = value; },
    get loadIfNeeded() { return loadIfNeeded; },
    get preferWorker() { return preferWorker; },
    get transport() { return transport; },
    get memoFetch() { return memoFetch; },
    set memoFetch(value: WasmClientContext["memoFetch"]) { memoFetch = value; },
    get persist() { return persist; },
    get transparentScan() { return transparentScan; },
    set transparentScan(value: WasmClientContext["transparentScan"]) { transparentScan = value; },
    get localLight() { return localLight; },
    get syncOpts() { return syncOpts; },
    get grpcWeb() { return grpcWeb; },
    get lwdPipe() { return lwdPipe; },
    get prewarmProvingKey() { return prewarmProvingKey; },
    set prewarmProvingKey(value: WasmClientContext["prewarmProvingKey"]) { prewarmProvingKey = value; },
    get client() { return client; },
    get rootsChecked() { return rootsChecked; },
    set rootsChecked(value: WasmClientContext["rootsChecked"]) { rootsChecked = value; },
    get refreshIfStale() { return refreshIfStale; },
    get transparentScanStatus() { return transparentScanStatus; },
    set transparentScanStatus(value: WasmClientContext["transparentScanStatus"]) { transparentScanStatus = value; },
    get sharedMemoStatus() { return sharedMemoStatus; },
    set sharedMemoStatus(value: WasmClientContext["sharedMemoStatus"]) { sharedMemoStatus = value; },
    get selectiveMemoStatus() { return selectiveMemoStatus; },
    set selectiveMemoStatus(value: WasmClientContext["selectiveMemoStatus"]) { selectiveMemoStatus = value; },
    get memoAbort() { return memoAbort; },
    set memoAbort(value: WasmClientContext["memoAbort"]) { memoAbort = value; },
    get rescanning() { return rescanning; },
    get disposing() { return disposing; },
    set disposing(value: WasmClientContext["disposing"]) { disposing = value; },
    get beginReservationDrain() { return beginReservationDrain; },
    get adoptSaved() { return adoptSaved; },
    get disposeWaits() { return disposeWaits; },
    get applyLoaded() { return applyLoaded; },
    get copyLegacy() { return copyLegacy; },
    get refreshMempool() { return refreshMempool; },
  };
  const { copyLegacy, applyLoaded, persist, adoptSaved, refreshIfStale, peekedNetwork, loadIfNeeded, preferWorker, persistReplacement, releaseReplacement, replaceWallet } = createStorageController(ctx);
  const { report, paintNoteBalance, runSync, refreshUtxos, refreshMempool, fetchMemos } = createSyncController(ctx);
  const { transparentSendSource, proveAndSubmit, proveAndSubmitHere, hardwareSubmit, requireSeed, finishSpend, parseFeeJson, parseMaxJson } = createSpendingController(ctx);

  const client: runtime.WasmClient = {
    lock() {
      spendingSeed = null;
      runtime.runtimeState.moduleSpendingSeed = null;
      runtime.saveSessionSeed(null);
    },
    async dispose() {
      disposing = true;
      disposeWaits.abort(new DOMException("wallet closing before broadcast", "AbortError"));
      runtime.interruptWasmSync();
      client.lock();
      memoAbort?.abort();
      runtime.runtimeState.persistOnHide = null;
      runtime.runtimeState.unbindHide?.();
      offRuntime();
      bus.clear();
      // Keep the session and its operation valid until an unsubmitted durable
      // reservation is rolled back. Submitted sends resolve this barrier before
      // awaiting the server, so an unknown acknowledgement cannot trap close.
      try {
        await reservationDrain;
      } finally {
        // Only invalidate snapshot writes after the rollback has committed (or
        // surfaced an error). Earlier cancellation can leave an unsubmitted
        // signed transaction in storage for a future sync to rebroadcast.
        runtime.cancelWasmSync();
        // A failed rollback commit still retires the in-memory owner. Preserve
        // its error and durable bytes, but never keep a closed session alive.
        cancelWalletOperation(sessionOperation ?? latestOperation);
        session = null;
        sessionOperation = null;
        await forgetScanWorkerWallet();
        await Promise.allSettled([...(loadLock ? [loadLock] : []), ...runtime.pendingReplacementCompletions]);
      }
    },
    baseUrl: `wasm:${network}`,
    on: (event, handler) => {
      const off = bus.on(event, handler);
      if (event === "runtime") {
        const current = wasmRuntime();
        if (current) {
          try { (handler as WalletEventHandler<"runtime">)({ ...current }); }
          catch { console.warn("runtime event handler"); }
        }
      }
      return off;
    },
    off: (event, handler) => bus.off(event, handler),
    health: async (): Promise<EngineHealth> => ({
      ok: true,
      wallet: !!((session && await session.scannedHeight().then(() => true, () => false)) || (await runtime.peekWasmWallet())),
      bind: "wasm",
      mode: "wasm-snapshot",
    }),
    forgetSavedWallet: (forgetOpts) => {
      if (forgetOpts?.pending !== undefined && forgetOpts.pending !== "reject") {
        return Promise.reject(new WalletError("unknown", "Invalid pending-payment removal policy."));
      }
      if (forgetOpts?.pending !== "reject") return runtime.forgetWasmWallet(forgetOpts);
      return runtime.withOriginSpendLock(async () => {
        if (disposing || syncLock || memoLock || rescanning) throw new WalletError("busy", "wallet work is in progress");
        if (!currentSession() && !(await loadIfNeeded())) {
          const generation = await readWalletGeneration();
          return runtime.forgetWasmWallet({ ...forgetOpts, expected: { generation, key: null } });
        }
        const source = session!;
        const operation = sessionOperation!;
        await operation.ready();
        await refreshIfStale(source, operation);
        assertSource(operation, source);
        if ((await source.pendingRawTxs()).length) throw new WalletError("forget_pending", "pending payment prevents local removal");
        // Legacy bytes have no revision token. Migrate via the existing guarded save first.
        if (!savedKeySeen) await persist();
        assertSource(operation, source);
        if (!savedKeySeen || storageGeneration === undefined) throw new WalletError("wallet_db", "wallet revision unavailable for removal");
        return runtime.forgetWasmWallet({ ...forgetOpts, expected: { generation: storageGeneration, key: savedKeySeen } });
      });
    },
    loadSavedWallet: () => runtime.withOriginSpendLock(async () => {
      const operation = captureWalletOperation();
      await operation.ready();
      const record = await readSavedSnapshotRecord(operation.signal);
      operation.assertCurrent();
      const preview = record && runtime.peekSnapshotBytes(record.bytes);
      if (preview?.network && preview.network !== network)
        throw new WalletError("invalid_network", "The saved wallet uses a different network.");
      const source = currentSession();
      if (source && (!record || record.generation !== storageGeneration)) {
        // Loading a replacement is read-only: retire the old owner/seed without
        // advancing or deleting the durable wallet generation. The spend lock
        // prevents this from tearing down an active proof or reservation.
        startReplacement();
      } else if (source && record && (!record.key || record.key !== savedKeySeen)) {
        await adoptSaved(source, operation);
      }
      if (!record) return null;
      if (!(await loadIfNeeded())) throw new WalletError("wallet_changed", "The saved wallet changed while loading.");
      const active = sessionOperation ?? captureWalletOperation();
      active.assertCurrent();
      const value = await snap();
      const key = await readSavedSnapshotKey(active.signal);
      active.assertCurrent();
      if (key !== savedKeySeen)
        throw new WalletError("wallet_changed", "The saved wallet changed while loading.");
      return value;
    }),
    getWallet: async () => {
      try {
        if (currentSession()) return snap();
        const peeked = await runtime.peekWasmWallet();
        const loading = loadIfNeeded();
        if (peeked) {
          void loading.catch((_e) => console.warn("wallet hydrate"));
          return peeked;
        }
        if (
          !(await runtime.withTimeout(
            loading,
            runtime.HYDRATE_MS + runtime.WORKER_START_MS,
            "restoring snapshot timed out — hide this overlay, wipe scan, or forget this device",
          ))
        ) {
          throw new Error("no wasm wallet");
        }
        return snap();
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (/no wasm wallet/i.test(msg)) throw WalletError.fromUnknown(e);
        throw WalletError.fromMessage(`could not load saved wallet: ${msg}`, e);
      }
    },
    sync: () =>
      runSync()
        .then((w) => {
          const bal = balanceEvent(w);
          if (bal) bus.emit("balance", bal);
          return w;
        })
        .catch((e) => {
          throw WalletError.fromUnknown(e);
        }),
    shield: async (threshold) => {
      assertReplacementSaved();
      if (!currentSession() && !(await loadIfNeeded())) throw new Error("no wasm wallet");
      const seed = requireSeed("shield");
      const operation = sessionOperation!;
      const source = session!;
      if (transparentScan === "compact") {
        const wallet = await snap();
        if (!wallet.transparentScanComplete || transparentScanStatus !== "complete") {
          throw new Error("Deposit scanning must finish before shielding. Sync the wallet first.");
        }
      } else {
        if (!transport.utxos) throw new Error("Transparent scan needs GetAddressUtxos, but address lookup is disabled");
        await refreshUtxos();
      }
      const txid = await proveAndSubmit("shield", seed, {
        thresholdZat: Number(threshold ?? 100_000),
      });
      return finishSpend(txid, operation, source);
    },
    send: async (to, amountZec, memo, sendOpts) => {
      if (!currentSession() && !(await loadIfNeeded())) throw new Error("no wasm wallet");
      const operation = sessionOperation!;
      const source = session!;
      const dest = to.trim();
      if (dest.toLowerCase().startsWith("zcash:")) {
        parseZip321(dest);
      }
      let txid: string | undefined;
      if (sendOpts?.signer) {
        txid = await hardwareSubmit({ to: dest, amountZec, ...(memo ? { memo } : {}) }, { ...sendOpts, signer: sendOpts.signer });
      } else {
        if ((await snap()).hardware) {
          throw new Error("this wallet signs on its hardware wallet: pass { signer } to send");
        }
        txid = await proveAndSubmit("send", requireSeed("send"), { to: dest, amountZec, memo },
          () => !sendOpts?.signal?.aborted && (sendOpts?.beforeBroadcast?.() ?? true));
      }
      return finishSpend(txid, operation, source);
    },
    supportsTransparentSend: async () => (await transparentSendSource())?.supported ?? false,
    sendTransparent: async (to, amountZec, options) => {
      const maxFeeZat = options?.maxFeeZat;
      const beforeBroadcast = options?.beforeBroadcast;
      if (maxFeeZat !== undefined && (!/^(0|[1-9]\d{0,15})$/.test(maxFeeZat) || BigInt(maxFeeZat) > 2_100_000_000_000_000n)) {
        throw new Error("invalid maximum fee in zatoshis");
      }
      const ready = await transparentSendSource();
      if (!ready?.supported) throw new Error("transparent swap outputs are unavailable for this wallet or WASM build");
      const { source, operation } = ready;
      assertSource(operation, source);
      const seed = requireSeed("send a transparent swap deposit");
      const txid = await runtime.withOriginSpendLock(() => {
        assertSource(operation, source);
        return proveAndSubmitHere("sendTransparent", seed, { to: to.trim(), amountZec, maxFeeZat }, beforeBroadcast);
      });
      return finishSpend(txid, operation, source);
    },
    estimateTransparentFee: async (to, amountZec) => {
      const ready = await transparentSendSource();
      if (!ready?.supported) throw new Error("transparent swap outputs are unavailable for this wallet or WASM build");
      const { source, operation } = ready;
      assertSource(operation, source);
      try {
        const raw = await source.estimateTransparentFee!(to.trim(), amountZec);
        assertSource(operation, source);
        return parseFeeJson(raw);
      } catch (e) { throw WalletError.fromUnknown(e); }
    },
    estimateFee: async (to, amountZec, memo) => {
      if (!currentSession() && !(await loadIfNeeded())) throw WalletError.fromMessage("no wasm wallet");
      const dest = to.trim();
      if (dest.toLowerCase().startsWith("zcash:")) parseZip321(dest);
      try {
        const raw = await session!.estimateFee(dest, amountZec ?? "", memo);
        return parseFeeJson(raw);
      } catch (e) {
        throw WalletError.fromUnknown(e);
      }
    },
    maxSend: async (to) => {
      if (!currentSession() && !(await loadIfNeeded())) throw WalletError.fromMessage("no wasm wallet");
      try {
        const raw = await session!.maxSend(to?.trim() || undefined);
        return parseMaxJson(raw);
      } catch (e) {
        throw WalletError.fromUnknown(e);
      }
    },
    inspectAddress: async (encoded) => {
      const inspect = runtime.requireBindings().inspectAddress;
      if (typeof inspect !== "function") {
        throw WalletError.fromMessage("this wasm build cannot inspect addresses");
      }
      try {
        return inspect(encoded.trim());
      } catch (e) {
        throw WalletError.fromUnknown(e);
      }
    },
    create: async (net, birthday, createOpts) => {
      if (birthday !== undefined) runtime.validateBirthdayInput(birthday);
      if (opts.requireExplicitReplacement && !createOpts?.replace && await runtime.peekWasmWallet()) {
        throw new WalletError("already_exists", "A saved wallet exists. Confirm replacement explicitly.");
      }
      const operation = startReplacement();
      runtime.replacementConsent.set(operation.signal, createOpts?.replace === true);
      const mnemonic = runtime.requireBindings().generateMnemonic();
      let passkeyWrite: runtime.PasskeyWrite | undefined;
      let claimed: runtime.ClaimedGeneration | undefined;
      try {
        // Register before any other await so the click still counts as a WebAuthn user gesture.
        if (createOpts?.passkey) passkeyWrite = await runtime.persistPasskeyIfBrowser(mnemonic, operation);
        let tip: number;
        if (typeof birthday === "number") {
          // Already validated. An explicit creation height is fully local;
          // sync checks the selected server later.
          tip = birthday;
        } else {
          try {
            tip = await transport.tip();
          } catch (e) {
            throw new WalletError("transport", `light server tip failed (${transport.label})`, e);
          }
        }
        operation.assertCurrent();
        const bday = await runtime.resolveBirthday(birthday, tip, Math.max(1, tip - 100), { network: net, regtestNu7Height: runtime.runtimeState.workerRegtestNu7 });
        operation.assertCurrent();
        if (createOpts?.beforeCommit) {
          await operation.ready();
          // Preparation must precede the durable replacement. A page
          // can disappear while awaiting UI; the prior wallet must stay readable.
          const previewWallet = runtime.requireBindings().WasmWallet.create(net, mnemonic, bday, 0);
          let preview: WalletSnapshot;
          try { preview = decorateSnapshot(JSON.parse(previewWallet.snapshotJson(serverLabel))); }
          finally { previewWallet.free?.(); }
          await abortable(operation.signal, async () => createOpts.beforeCommit!({
            wallet: preview, recoveryPhrase: mnemonic, signal: operation.signal,
          }));
          operation.assertCurrent();
        }
        claimed = await replaceWallet(
          operation,
          (s) => s.create(net, mnemonic, bday, 0),
          () => runtime.requireBindings().WasmWallet.create(net, mnemonic, bday, 0),
        );
        operation.assertCurrent();
        spendingSeed = mnemonic;
        runtime.runtimeState.moduleSpendingSeed = mnemonic;
        if (unlockPolicy !== "each-spend") runtime.saveSessionSeed(mnemonic);
        // Capture the return value before committing. A post-commit metadata
        // read must not strand a newly created wallet without its phrase.
        const w: CreationResult = await snap();
        operation.assertCurrent();
        const createdSource = session;
        const createdGeneration = claimed.generation;
        const stillOwnsCreation = () => session === createdSource && sessionOperation === operation && !operation.signal.aborted;
        w.mnemonic = mnemonic;
        w.unlockPolicy = unlockPolicy;
        await persistReplacement(claimed);
        claimed = undefined;
        passkeyWrite = undefined;
        // The captured phrase must be delivered once durable creation wins,
        // even if close/cancellation raced the transaction's completion event.
        const pass = createOpts?.passphrase?.trim();
        if (pass) {
          w.localSeedBackup = "not-saved";
          if (stillOwnsCreation() && walletStorageAvailable()) {
            try {
              await runtime.persistSeedIfBrowser(pass, mnemonic, operation, createdGeneration);
              w.localSeedBackup = "saved";
            } catch { /* Creation is committed; return the phrase and explicit backup outcome. */ }
          }
        }
        if (w.unlockPolicy === "each-spend" && stillOwnsCreation()) client.lock();
        return w;
      } catch (e) {
        if (claimed) await releaseReplacement(operation, claimed);
        await runtime.undoPasskeyIfBrowser(passkeyWrite, operation);
        throw e;
      }
    },
    restore: async (mnemonic, net, birthday, restoreOpts) => {
      if (birthday !== undefined) runtime.validateBirthdayInput(birthday);
      const words = mnemonic.trim();
      if (/^uview/i.test(words)) {
        return client.restoreUfvk(words, net, birthday, restoreOpts);
      }
      if (restoreOpts?.signal || restoreOpts?.assertCurrent || restoreOpts?.beforeCommit) {
        throw new Error("guarded restore requires browser restoreUfvk");
      }
      // Validate before preparing a replacement or changing the passkey's phrase.
      runtime.assertRecoveryPhrase(words, net);
      if (opts.requireExplicitReplacement && !restoreOpts?.replace && await runtime.peekWasmWallet()) {
        throw new WalletError("already_exists", "A saved wallet exists. Confirm replacement explicitly.");
      }
      const operation = startReplacement();
      runtime.replacementConsent.set(operation.signal, restoreOpts?.replace === true);
      let passkeyWrite: runtime.PasskeyWrite | undefined;
      let claimed: runtime.ClaimedGeneration | undefined;
      try {
        if (restoreOpts?.passkey) passkeyWrite = await runtime.persistPasskeyIfBrowser(words, operation);
        let tip: number;
        try {
          tip = await transport.tip();
          operation.assertCurrent();
        } catch (e) {
          throw new Error(
            `light server tip failed (${transport.label}): ${e instanceof Error ? e.message : e}`,
          );
        }
        const bday = await runtime.resolveBirthday(birthday, tip, net === "regtest" ? 1 : 1, { network: net, regtestNu7Height: runtime.runtimeState.workerRegtestNu7 });
        if (bday > tip) throw new Error(`birthday ${bday} is above tip ${tip}`);
        operation.assertCurrent();
        claimed = await replaceWallet(
          operation,
          (s) => s.create(net, words, bday, 0),
          () => runtime.requireBindings().WasmWallet.create(net, words, bday, 0),
        );
        operation.assertCurrent();
        await persistReplacement(claimed, restoreOpts?.unlockPolicy);
        claimed = undefined;
        passkeyWrite = undefined;
        operation.assertCurrent();
        if (restoreOpts?.unlockPolicy) {
          applyUnlockPolicy(restoreOpts.unlockPolicy);
        }
        operation.assertCurrent();
        spendingSeed = words;
        runtime.runtimeState.moduleSpendingSeed = words;
        if (unlockPolicy !== "each-spend") runtime.saveSessionSeed(words);
        const pass = restoreOpts?.passphrase?.trim();
        if (pass) await runtime.persistSeedIfBrowser(pass, words, operation, storageGeneration!);
        const w = await snap();
        operation.assertCurrent();
        w.unlockPolicy = unlockPolicy;
        if (unlockPolicy === "each-spend") client.lock();
        return w;
      } catch (e) {
        if (claimed) await releaseReplacement(operation, claimed);
        await runtime.undoPasskeyIfBrowser(passkeyWrite, operation);
        throw e;
      }
    },
    restoreUfvk: async (ufvk, net, birthday, restoreOpts) => {
      if (birthday !== undefined) runtime.validateBirthdayInput(birthday);
      runtime.assertViewingKey(ufvk.trim(), net);
      if (opts.requireExplicitReplacement && !restoreOpts?.replace && await runtime.peekWasmWallet()) {
        throw new WalletError("already_exists", "A saved wallet exists. Confirm replacement explicitly.");
      }
      restoreOpts?.signal?.throwIfAborted();
      restoreOpts?.assertCurrent?.();
      // Stop calling the UI guard after a successful commit. The wallet lease
      // outlives the recovery screen and is still used by ordinary sync/saves.
      let guarding = true;
      const operation = startReplacement(() => { if (guarding) restoreOpts?.assertCurrent?.(); });
      runtime.replacementConsent.set(operation.signal, restoreOpts?.replace === true);
      const abort = () => { cancelWalletOperation(operation, restoreOpts?.signal?.reason); };
      restoreOpts?.signal?.addEventListener("abort", abort, { once: true });
      let claimed: runtime.ClaimedGeneration | undefined;
      try {
        let tip: number;
        try {
          tip = await transport.tip(operation.signal);
          operation.assertCurrent();
        } catch (e) {
          operation.assertCurrent();
          throw new Error(
            `light server tip failed (${transport.label}): ${e instanceof Error ? e.message : e}`,
          );
        }
        const bday = await runtime.resolveBirthday(birthday, tip, net === "regtest" ? 1 : 1, { network: net, regtestNu7Height: runtime.runtimeState.workerRegtestNu7 });
        if (bday > tip) throw new Error(`birthday ${bday} is above tip ${tip}`);
        operation.assertCurrent();
        claimed = await replaceWallet(
          operation,
          (s) => s.fromUfvk(net, ufvk.trim(), bday, 0),
          () => {
            const fromUfvk = runtime.requireBindings().WasmWallet.fromUfvk;
            if (typeof fromUfvk !== "function") {
              throw new Error("this wasm build cannot restore from UFVK");
            }
            return fromUfvk(net, ufvk.trim(), bday, 0);
          },
        );
        operation.assertCurrent();
        const w = await snap();
        operation.assertCurrent();
        w.viewOnly = true;
        w.unlockPolicy = restoreOpts?.unlockPolicy ?? unlockPolicy;
        await restoreOpts?.beforeCommit?.(w);
        operation.assertCurrent();
        spendingSeed = null;
        runtime.runtimeState.moduleSpendingSeed = null;
        runtime.saveSessionSeed(null);
        // This is the last asynchronous step. All caller preparation and
        // snapshot/history reads finish before the atomic durable replacement.
        await persistReplacement(claimed, restoreOpts?.unlockPolicy);
        claimed = undefined;
        guarding = false;
        if (restoreOpts?.unlockPolicy) applyUnlockPolicy(restoreOpts.unlockPolicy);
        return w;
      } catch (e) {
        cancelWalletOperation(operation, e);
        if (claimed) await releaseReplacement(operation, claimed);
        throw e;
      } finally {
        guarding = false;
        restoreOpts?.signal?.removeEventListener("abort", abort);
      }
    },
    restoreHardware: async (account: HardwareAccount, net, birthday, restoreOpts) => {
      if (birthday !== undefined) runtime.validateBirthdayInput(birthday);
      const ufvk = account.ufvk.trim();
      runtime.assertViewingKey(ufvk, net);
      const fromHardware = runtime.requireBindings().WasmWallet.fromHardware;
      if (typeof fromHardware !== "function") throw new Error("this wasm build has no hardware-wallet support");
      // Validate before claiming the wallet slot, which fences out the saved wallet.
      fromHardware(net, ufvk, 1, account.device, account.seedFingerprint.trim(), account.accountIndex).free?.();
      if (opts.requireExplicitReplacement && !restoreOpts?.replace && await runtime.peekWasmWallet()) {
        throw new WalletError("already_exists", "A saved wallet exists. Confirm replacement explicitly.");
      }
      const operation = startReplacement();
      runtime.replacementConsent.set(operation.signal, restoreOpts?.replace === true);
      let tip: number;
      try {
        tip = await transport.tip();
        operation.assertCurrent();
      } catch (e) {
        throw new Error(
          `light server tip failed (${transport.label}): ${e instanceof Error ? e.message : e}`,
        );
      }
      const bday = await runtime.resolveBirthday(birthday, tip, 1, { network, regtestNu7Height: runtime.runtimeState.workerRegtestNu7 });
      if (bday > tip) throw new Error(`birthday ${bday} is above tip ${tip}`);
      operation.assertCurrent();
      const input = {
        device: account.device,
        ufvk,
        seedFingerprint: account.seedFingerprint.trim(),
        accountIndex: account.accountIndex,
      };
      const claimed = await replaceWallet(
        operation,
        (s) => s.fromHardware(net, input, bday),
        () => fromHardware(net, ufvk, bday, input.device, input.seedFingerprint, input.accountIndex),
      );
      try {
        operation.assertCurrent();
        spendingSeed = null;
        runtime.runtimeState.moduleSpendingSeed = null;
        runtime.saveSessionSeed(null);
        await persistReplacement(claimed);
      } catch (e) {
        await releaseReplacement(operation, claimed);
        throw e;
      }
      operation.assertCurrent();
      const w = await snap();
      operation.assertCurrent();
      w.unlockPolicy = unlockPolicy;
      return w;
    },
    attachSeed: async (mnemonic, attachOpts) => {
      const operation = captureWalletOperation();
      const assertUnlock = () => { operation.assertCurrent(); attachOpts?.signal?.throwIfAborted(); };
      assertUnlock();
      await operation.ready();
      if (!currentSession() && !(await loadIfNeeded())) throw new Error("no wasm wallet");
      operation.assertCurrent();
      const source = session!;
      assertUnlock();
      const words = mnemonic.trim();
      // A hardware account spends only through its device.
      if ((await snap()).hardware) {
        throw new Error("this is a hardware-wallet account: spends are signed on the device, not with a phrase");
      }
      // Rust verifies account ownership. Never swallow an engine failure and
      // accept an arbitrary phrase when viewing-key metadata is missing.
      assertUnlock();
      await source.attachSeed(words);
      assertSource(operation, source);
      assertUnlock();
      spendingSeed = words;
      runtime.runtimeState.moduleSpendingSeed = words;
      if (unlockPolicy !== "each-spend") runtime.saveSessionSeed(words);
      await persist();
      assertUnlock();
      const w = await snap();
      assertUnlock();
      w.viewOnly = false;
      return w;
    },
    setUnlockPolicy: async (policy) => {
      const operation = captureWalletOperation();
      await operation.ready();
      const expected = storageGeneration ?? await readWalletGeneration(operation.signal);
      await updateUnlockPolicy(policy, operation, expected);
      if (session) {
        const w = await snap();
        w.unlockPolicy = policy;
        return w;
      }
    },
    unlockPolicy: () => unlockPolicy,
    hasSpendingSeed: () => !!(spendingSeed || runtime.runtimeState.moduleSpendingSeed),
    fetchMemos,
    setMemoFetch: (mode) => {
      if (mode !== memoFetch) {
        memoAbort?.abort();
        selectiveMemoStatus = mode === "on-demand" ? "off" : "scanning";
        sharedMemoStatus = mode === "shared" ? "scanning" : "off";
      }
      memoFetch = mode;
    },
    setTransparentScan: (mode) => {
      if (mode !== transparentScan) transparentScanStatus = mode === "compact" ? "scanning" : "off";
      transparentScan = mode;
    },
    setPrewarmProvingKey: (enabled) => { prewarmProvingKey = enabled; },
    history: (limit = 50, query) => loadHistory(limit, query),
    transaction: async (txid) => {
      const rows = await loadHistory(1, { txid });
      return rows[0] ?? null;
    },
    pending: (limit = 50) => loadHistory(limit, { status: "pending" }),
    tip: async () => {
      const scanned = currentSession()
        ? await session!.scannedHeight()
        : (await loadIfNeeded())
          ? await session!.scannedHeight()
          : 0;
      const t = await transport.tip();
      return { tip: t, scanned, behind: Math.max(0, t - scanned) } satisfies ChainTip;
    },
    probeSetup: async (net, server, rpc) => {
      let tip: number | undefined;
      let chain = net;
      let ok = true;
      let error: string | undefined;
      try {
        if (transport.info) {
          const inf = await transport.info();
          if (inf.chain) chain = inf.chain;
          tip = inf.blockHeight;
        }
        if (tip == null) tip = await transport.tip();
      } catch (e) {
        ok = false;
        error = e instanceof Error ? e.message : String(e);
      }
      return {
        network: net,
        light: {
          ok,
          url: server || transport.label,
          chain,
          tip,
          tScan: !!transport.utxos,
          error,
        },
        validator: {
          ok: !!rpc,
          url: rpc || "",
          chain: "",
          error: rpc ? undefined : "Zakura RPC is independent; not required to scan",
        },
        defaults: {
          light: net === "regtest" ? "http://127.0.0.1:28137" : "http://127.0.0.1:8137",
          validatorRpc:
            net === "regtest"
              ? "http://127.0.0.1:29232"
              : net === "testnet"
                ? "http://127.0.0.1:28232"
                : "http://127.0.0.1:8232",
          publicLight:
            net === "testnet"
              ? "https://testnet.zec.rocks:443"
              : net === "mainnet"
                ? "https://zec.rocks:443"
                : "http://127.0.0.1:28137",
        },
      };
    },
    saveSetup: async () => {
      throw new Error("wasm snapshot does not persist native server URLs; use the lab light/RPC fields");
    },
    nextAddress: async () => {
      if (!currentSession() && !(await loadIfNeeded())) throw new Error("no wasm wallet");
      assertReplacementSaved();
      await session!.nextUnifiedAddress();
      await persist();
      return snap();
    },
    rescan: async ({ birthday }) => {
      runtime.validateBirthdayInput(birthday);
      if (typeof birthday === "string" && (!birthday.trim() || birthday.trim().toLowerCase() === "auto")) {
        throw new WalletError("invalid_birthday", "rescan requires an explicit birthday");
      }
      assertReplacementSaved();
      if (disposing || syncLock || memoLock || runtime.runtimeState.spendingOperation) {
        throw new WalletError("busy", "another wallet operation is in progress");
      }
      rescanning = true;
      try {
        return await runtime.withOriginSpendLock(async () => {
          if (!currentSession() && !(await loadIfNeeded())) throw new WalletError("not_found", "no wasm wallet");
          const source = session!;
          const operation = sessionOperation!;
          await operation.ready();
          assertSource(operation, source);
          if (disposing || runtime.runtimeState.spendingOperation) throw new WalletError("busy", "another wallet operation is in progress");
          runtime.runtimeState.spendingOperation = operation.signal;
          let backup: Uint8Array | undefined;
          let epoch = stateEpoch;
          try {
            await refreshIfStale(source, operation);
            assertSource(operation, source);
            const tip = await transport.tip(operation.signal);
            const height = await runtime.resolveBirthday(birthday, tip, 1, { network, regtestNu7Height: runtime.runtimeState.workerRegtestNu7 });
            assertSource(operation, source);
            if (height > tip) throw new WalletError("birthday_above_tip", "birthday is above chain tip");
            if (height > await source.birthday()) throw new WalletError("rescan_later_birthday", "rescan later birthday");
            if ((await source.pendingRawTxs()).length) throw new WalletError("rescan_pending", "rescan pending payment");
            if (tip - height + 1 > runtime.MAX_GAP && !opts.allowDeepSync) throw new WalletError("deep_sync_rejected", "deep sync requires explicit opt-in");
            // A reset must never remove reservations for unknown broadcast outcomes.
            // Drain any earlier coalesced/pagehide writes before mutating state.
            await persist();
            assertSource(operation, source);
            backup = await source.toSnapshot();
            assertSource(operation, source);
            epoch = stateEpoch;
            const baseKey = savedKeySeen;
            await source.rescanFrom(height);
            // The session survives, but its replacement trees no longer contain
            // the cached roots. A rollback may safely fetch those roots again.
            rootsChecked = null;
            assertSource(operation, source);
            await persist({ epoch, baseKey });
            assertSource(operation, source);
            // Coverage was cleared by the committed reset. Failed saves keep
            // the previous session's completion statuses during rollback.
            transparentScanStatus = transparentScan === "compact" ? "scanning" : "off";
            sharedMemoStatus = memoFetch === "shared" ? "scanning" : "off";
            selectiveMemoStatus = memoFetch === "on-demand" ? "off" : "scanning";
          } catch (error) {
            // wallet_changed already adopted the other tab's committed snapshot.
            // Close/forget/replacement have retired this source; never revive it.
            if (backup && !operation.signal.aborted && source === session && stateEpoch === epoch) {
              try { await source.fromSnapshot(backup); }
              catch { retireFailedSession(source); }
            }
            throw error;
          } finally {
            if (runtime.runtimeState.spendingOperation === operation.signal) runtime.runtimeState.spendingOperation = null;
          }
          return snap();
        });
      } finally { rescanning = false; }
    },
    resetScan: async () => {
      assertReplacementSaved();
      // Transfer this handle to a new owner before aborting its old owner. Other
      // clients and in-flight proofs are invalidated; the handle we reset survives.
      const previous = currentSession();
      session = null;
      sessionOperation = null;
      const operation = beginWalletOperation();
      latestOperation = operation;
      runtime.replacementConsent.set(operation.signal, true);
      runtime.cancelWasmSync();
      if (previous) publishSession(previous, operation, storageGeneration!);
      await operation.ready();
      if (previous) {
        try {
          assertSource(operation, previous);
          await runtime.withTimeout(previous.resetScan(), 15_000, "resetScan timed out");
          operation.assertCurrent();
          await persist();
          operation.assertCurrent();
          return snap();
        } catch {
          operation.assertCurrent();
          console.warn("resetScan on session failed; rebuilding from UFVK");
        }
      }
      let src: WalletSnapshot | null = null;
      try {
        if (session) src = await snap();
      } catch {
        src = null;
      }
      if (!src) {
        const bytes = await runtime.withTimeout(idbGet(), runtime.IDB_READ_MS, "IndexedDB wallet read timed out").catch(
          () => null,
        );
        src = bytes ? runtime.peekSnapshotBytes(bytes) : null;
      }
      operation.assertCurrent();
      if (!src?.ufvk) throw new Error("no wasm wallet");
      const ufvk = src.ufvk;
      const net = peekedNetwork(src.network);
      const bday = src.birthdayHeight > 0 ? src.birthdayHeight : 1;
      // A hardware account must stay one: device, fingerprint and account index.
      const hw = src.hardware;
      const claimed = await replaceWallet(
        operation,
        (s) => hw
          ? s.fromHardware(net, { device: hw.device, ufvk, seedFingerprint: hw.seedFingerprint, accountIndex: hw.accountIndex }, bday)
          : s.fromUfvk(net, ufvk, bday, 0),
        () => {
          const { fromUfvk, fromHardware } = runtime.requireBindings().WasmWallet;
          if (hw) {
            if (typeof fromHardware !== "function") throw new Error("this wasm build cannot reset scan");
            return fromHardware(net, ufvk, bday, hw.device, hw.seedFingerprint, hw.accountIndex);
          }
          if (typeof fromUfvk !== "function") {
            throw new Error("this wasm build cannot reset scan");
          }
          return fromUfvk(net, ufvk, bday, 0);
        },
      );
      try {
        operation.assertCurrent();
        await persistReplacement(claimed);
      } catch (e) {
        await releaseReplacement(operation, claimed);
        throw e;
      }
      operation.assertCurrent();
      const w = await snap();
      w.viewOnly = !!src.viewOnly;
      return w;
    },
    waitUntilCaughtUp: async (wait?: WaitOpts) => {
      const timeoutMs = wait?.timeoutMs ?? 300_000;
      const startAt = Date.now();
      let last: ChainTip | null = null;
      while (Date.now() - startAt < timeoutMs) {
        await runSync();
        last = await client.tip();
        if (last.behind === 0) return last;
        await new Promise((r) => setTimeout(r, wait?.intervalMs ?? 1_000));
      }
      throw new Error(`still behind tip=${last?.tip} scanned=${last?.scanned}`);
    },
  };

  return client;
}
