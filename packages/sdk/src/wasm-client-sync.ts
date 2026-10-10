import type { SyncStage } from "@z-stack/core";
import {
  canShield,
  catchUpPercent,
  fmtSecs,
  grpcWebPrefetch,
  isWalletError,
  lightStallWarning,
  NEAR_TIP_BLOCKS,
  pipePrefetch,
  prefetchBuffer,
  SHIELD_THRESHOLD_ZAT,
  STALL_QUIET_REMAINING,
  SYNC_STALL_MS,
  syncTuning,
  WalletError,
  type HistoryEntry,
  type WalletSnapshot
} from "@z-stack/core";
import { createLightServerRecovery, LightServerUnavailableError, type LightServerRecovery } from "./light-server-recovery";
import { isTransientLightServerError } from "./lwd";
import { createBlockPrefetch } from "./prefetch";
import { supportsTransparentCompact, syncPublicData } from "./public-data";
import {
  isTreeConflictError,
  scanWorkerRuntime,
  treeConflictUserMessage,
  workerScanSession,
  type ScanSession
} from "./scan-host";
import { fetchSubtreeRoots, SUBTREE_ROOTS_PAGE } from "./subtree-roots";
import { captureWalletOperation } from "./wallet-lifecycle";
import { readWalletGeneration, walletStorageAvailable } from "./wallet-storage";
import type { WasmClientContext } from './wasm-client-context';
import * as runtime from './wasm-client-runtime';

/** Block scanning and optional public-data enhancement. */
export function createSyncController(ctx: Pick<WasmClientContext, "onProgress" | "bus" | "session" | "snap" | "currentSession" | "syncLock" | "memoLock" | "loadIfNeeded" | "preferWorker" | "transport" | "memoFetch" | "persist" | "opts" | "transparentScan" | "localLight" | "syncOpts" | "grpcWeb" | "lwdPipe" | "retireFailedSession" | "spendingSeed" | "prewarmProvingKey" | "sessionOperation" | "client" | "unlockPolicy" | "rootsChecked" | "assertSource" | "network" | "refreshIfStale" | "stateEpoch" | "transparentScanStatus" | "sharedMemoStatus" | "selectiveMemoStatus" | "memoAbort" | "storageGeneration" | "rescanning">) {

  function report(p: runtime.WasmProgress): void {
    ctx.onProgress?.(p);
    emitSyncFromProgress(p);
  }

  function emitSyncFromProgress(p: runtime.WasmProgress): void {
    const stage: SyncStage =
      p.stage === "connecting" ||
        p.stage === "downloading" ||
        p.stage === "scanning" ||
        p.stage === "enhancing" ||
        p.stage === "synced"
        ? p.stage
        : "catching_up";
    ctx.bus.emit("sync", {
      stage,
      scanned: p.scanned,
      downloaded: p.downloaded,
      tip: p.tip,
      percent: p.percent,
      notesFound: p.notesFound,
      spendsFound: p.spendsFound,
      blocksPerSecond: p.blocksPerSecond,
      remainingSeconds: p.remainingSeconds,
      remainingHuman: p.remainingHuman,
      message: p.message,
      heading: p.heading,
    });
    if (p.paintWallet && p.availableZat != null) {
      ctx.bus.emit("balance", {
        availableZat: p.availableZat,
        pendingZat: p.pendingZat,
        orchardAvailable: p.orchardAvailable,
        totalZec: p.availableZec,
      });
    }
  }

  async function paintNoteBalance(partial: runtime.WasmProgress): Promise<void> {
    if (!ctx.session) {
      report(partial);
      return;
    }
    try {
      const w = await ctx.snap();
      report({
        ...partial,
        paintWallet: true,
        availableZat: w.balance.totalAvailable,
        pendingZat: w.balance.totalPending ?? 0,
        availableZec: w.balance.totalZec,
        pendingZec: w.balance.pendingZec,
        orchardAvailable: w.balance.orchardAvailable,
      });
    } catch {
      report(partial);
    }
  }

  function runSync(): Promise<WalletSnapshot> {
    if (ctx.rescanning) return Promise.reject(new WalletError("busy", "rescan is in progress"));
    ctx.currentSession();
    if (ctx.syncLock?.epoch === runtime.runtimeState.wasmSyncEpoch) return ctx.syncLock.promise;
    if (ctx.memoLock?.epoch === runtime.runtimeState.wasmSyncEpoch) {
      const operation = captureWalletOperation();
      const epoch = runtime.runtimeState.wasmSyncEpoch;
      return ctx.memoLock.promise.catch(() => { }).then(() => {
        operation.assertCurrent();
        if (epoch !== runtime.runtimeState.wasmSyncEpoch) throw new Error("sync cancelled");
        return runSync();
      });
    }
    const promise = runSyncInner();
    const entry = { epoch: runtime.runtimeState.wasmSyncEpoch, promise };
    ctx.syncLock = entry;
    const release = () => { if (ctx.syncLock === entry) ctx.syncLock = null; };
    void promise.then(release, release);
    return promise;
  }

  async function runSyncInner(): Promise<WalletSnapshot> {
    const g = runtime.runtimeState.wasmSyncEpoch;
    const signal = runtime.runtimeState.syncAbort.signal;
    const cancelled = () => g !== runtime.runtimeState.wasmSyncEpoch;
    report({
      stage: "connecting",
      heading: ctx.session ? "Catching up" : "Restoring snapshot",
      message: ctx.session ? "reading chain tip" : "hydrating snapshot",
      scanned: 0,
      tip: 0,
      notesFound: 0,
      spendsFound: 0,
      percent: ctx.session ? 4 : 8,
    });
    if (!ctx.session) {
      const ok = await runtime.withTimeout(
        ctx.loadIfNeeded(),
        runtime.HYDRATE_MS + runtime.WORKER_START_MS,
        "restoring snapshot timed out — hide this overlay, wipe scan, or forget this device",
      );
      if (!ok || !ctx.session) throw new Error("no wasm wallet — create or restore first");
    }
    await paintNoteBalance({
      stage: "connecting",
      heading: "Catching up",
      message: "available balance from decrypted notes",
      scanned: await ctx.session.scannedHeight(),
      tip: 0,
      notesFound: 0,
      spendsFound: 0,
      percent: 8,
      paintWallet: true,
    });
    await ctx.preferWorker();
    if (cancelled()) throw new Error("sync cancelled");
    const source = ctx.session!;
    let recoveryScanned = await source.scannedHeight();
    let recoveryStart = recoveryScanned;
    let recoveryTip = 0;
    let notesFound = 0;
    let spendsFound = 0;
    const recovery = createLightServerRecovery({
      signal,
      cancelled,
      onWaiting: () => report({
        stage: "connecting",
        heading: "Waiting for light server",
        scanned: recoveryScanned,
        downloaded: recoveryScanned,
        tip: recoveryTip,
        notesFound,
        spendsFound,
        percent: recoveryTip > 0 ? catchUpPercent(recoveryStart, recoveryScanned, recoveryScanned, recoveryTip) : 8,
        message: "Connection lost. Retrying from the last scanned block…",
      }),
    });
    const tip = await recovery.run(() => ctx.transport.tip(signal));
    recoveryTip = tip;
    if (cancelled()) throw new Error("sync cancelled");
    const start = await source.nextHeight();
    recoveryScanned = start - 1;
    recoveryStart = start;
    const birthday = await source.birthday();
    if (start <= birthday && tip - start > 8_000) {
      console.warn("Sync restarted from the wallet birthday; checking the saved snapshot is recommended.");
    }
    const syncStarted = Date.now();
    const nearTip = tip - start <= NEAR_TIP_BLOCKS;
    const followOn = tip - start <= STALL_QUIET_REMAINING;
    const floorPct = nearTip ? 90 : 1;
    let lastMark = Math.max(start, 0);
    let lastMove = Date.now();
    const catchUpPct = (scanned: number, downloaded = scanned) =>
      catchUpPercent(start, scanned, downloaded, tip);
    report({
      stage: "connecting",
      heading: tip > start ? "Catching up" : "Connecting",
      scanned: start,
      downloaded: start,
      tip,
      notesFound: 0,
      spendsFound: 0,
      percent: floorPct,
      remainingHuman: tip > start ? `${tip - start} behind` : undefined,
      message:
        tip > start
          ? `${tip - start} blocks behind tip ${tip}`
          : `tip ${tip}`,
    });
    if (start > tip) {
      await seedBirthdayTrees(undefined, recovery, signal);
      await refreshPublicData(signal);
      await refreshPostScanState(recovery, signal);
      if (cancelled()) throw new Error("sync cancelled");
      // Failed/unavailable full transactions also need a retry when no new
      // compact block has arrived. Completed blank memos are excluded by Rust.
      if (ctx.memoFetch === "auto") await enhanceMemos(true);
      if (cancelled()) throw new Error("sync cancelled");
      await rebroadcastPending(source);
      await ctx.persist();
      const after = await ctx.snap();
      const scanned = after.scannedHeight ?? await source.scannedHeight();
      if (cancelled()) throw new Error("sync cancelled");
      report({
        stage: "synced",
        scanned,
        downloaded: tip,
        tip,
        notesFound,
        spendsFound,
        percent: 100,
        remainingSeconds: 0,
        remainingHuman: "done",
        message: "synced",
      });
      if (cancelled()) throw new Error("sync cancelled");
      warmProvingKeyAfterSync(after);
      return autoShieldSnapshot(after);
    }
    const gap = tip - start + 1;
    if (gap > runtime.MAX_GAP && !ctx.opts.allowDeepSync) {
      throw new Error(
        `unscanned range ${start}..${tip} is past the default deep sync limit of ${runtime.MAX_GAP} blocks; set deepSync: true (createWallet) to scan it`,
      );
    }
    // Lightwallet protocol >= 0.5 includes transparent inputs and outputs in the same compact
    // response. When coverage is aligned, scan both pools before checkpointing;
    // older snapshots with a deposit gap retain the independent backfill path.
    let combinedTransparent = false;
    if (ctx.transparentScan === "compact" && ctx.transport.transparentBlocks && source.applyTransparentBlocks) {
      const state = await ctx.snap();
      if ((state.transparentScanHeight ?? birthday - 1) === start - 1) {
        const info = await ctx.transport.info?.().catch(() => undefined);
        if (cancelled()) throw new Error("sync cancelled");
        combinedTransparent = !!info?.transparentCompact && supportsTransparentCompact(info.protocolVersion)
          && await source.supportsTransparentBlocks?.() === true;
        if (cancelled()) throw new Error("sync cancelled");
      }
    }
    const tuned = syncTuning(ctx.localLight, ctx.syncOpts);
    const shieldedPrefetch = ctx.grpcWeb
      ? grpcWebPrefetch(scanWorkerRuntime()?.threads, runtime.radioConstrained())
      : ctx.lwdPipe
        ? pipePrefetch(runtime.radioConstrained())
        : tuned.prefetch;
    // Dense all-pool ranges can exceed 14 MB per 1k blocks. Sixteen requests
    // queue behind browser sockets/Zaino and exhaust their response deadlines.
    // Bound both range size and concurrency, keeping the same checkpoint span.
    const prefetch = combinedTransparent ? Math.min(shieldedPrefetch, 4) : shieldedPrefetch;
    const batch = combinedTransparent ? Math.min(tuned.batch, 500) : tuned.batch;
    const persistEvery = Math.ceil(tuned.batch * tuned.persistEvery / batch);
    let batches = 0;
    let persistTail: Promise<void> = Promise.resolve();
    let checkpointActive = false;
    let lastCheckpointBatch = 0;

    const startCheckpoint = () => {
      if (cancelled() || signal.aborted) return;
      checkpointActive = true;
      lastCheckpointBatch = batches;
      const checkpoint = ctx.persist();
      persistTail = checkpoint;
      void checkpoint.then(
        () => finishCheckpoint(),
        (error) => finishCheckpoint(error),
      );
      function finishCheckpoint(error?: unknown) {
        checkpointActive = false;
        if (error && !cancelled()) console.warn("snapshot checkpoint failed");
        // Applying may have crossed several checkpoint boundaries while the
        // previous IndexedDB transaction waited. Capture the latest state once
        // it settles rather than postponing all durability until final save.
        if (!cancelled() && !signal.aborted && batches - lastCheckpointBatch >= persistEvery) {
          startCheckpoint();
        }
      }
    };

    const drainCheckpoints = async () => {
      while (checkpointActive) {
        await persistTail.catch(() => { });
        // A completed checkpoint can immediately schedule one follow-up.
        await Promise.resolve();
      }
    };

    const tick = (
      scanned: number,
      downloaded: number,
      fetching = 0,
      mode: "fetch" | "apply" | "idle" = "idle",
    ) => {
      const showDl = Math.max(downloaded, scanned);
      const remaining = Math.max(0, tip - scanned);
      const applying = mode === "apply";
      const fetchingNow = mode === "fetch" || fetching > 0;
      const elapsed = Math.max(0.25, (Date.now() - syncStarted) / 1000);
      const done = Math.max(0, scanned - (start - 1));
      const scanMoving = done > 1;
      const mark = Math.max(showDl, scanned);
      if (mark !== lastMark || applying) {
        // Completing an expensive WASM apply is progress even when download
        // was already ahead of this blob and the visible high-water mark did
        // not change.
        lastMark = mark;
        lastMove = Date.now();
      }
      const frozen = Date.now() - lastMove > SYNC_STALL_MS && remaining > 0;
      const span = Math.max(1, tip - start);
      // Snapshot serialization/IndexedDB commit can stop height movement while
      // fetch is healthy and the bounded prefetch buffer is full.
      const stalled = !checkpointActive && lightStallWarning(span, remaining, frozen, applying || fetchingNow);
      const bps = done / elapsed;
      const eta =
        scanMoving && remaining > 0 && remaining <= NEAR_TIP_BLOCKS
          ? remaining / Math.max(bps, 1)
          : 0;
      const stage =
        remaining === 0
          ? "scanning"
          : applying
            ? "scanning"
            : remaining <= NEAR_TIP_BLOCKS
              ? "scanning"
              : fetchingNow
                ? "downloading"
                : "scanning";
      report({
        stage,
        heading: stalled
          ? "Download stuck"
          : applying
            ? "Scanning compact blocks"
            : remaining > 0 && remaining <= NEAR_TIP_BLOCKS
              ? "Catching up"
              : remaining === 0
                ? "Scanning"
                : fetchingNow
                  ? "Downloading compact blocks"
                  : "Scanning compact blocks",
        scanned,
        downloaded: showDl,
        tip,
        notesFound,
        spendsFound,
        percent: catchUpPct(scanned, showDl),
        remainingSeconds: eta > 0 ? Math.max(1, Math.round(eta)) : undefined,
        remainingHuman: stalled
          ? "download not moving"
          : eta > 0
            ? fmtSecs(Math.max(1, Math.round(eta)))
            : remaining > 0 && nearTip
              ? `${remaining} behind`
              : undefined,
        blocksPerSecond: scanMoving ? Math.round(bps) : undefined,
        message: stalled
          ? `compact-block fetch not moving at ${showDl}/${tip}`
          : applying
            ? `applying ${scanned}/${tip} · ${notesFound} notes${checkpointActive ? " · saving checkpoint" : ""}`
            : remaining > 0 && remaining <= NEAR_TIP_BLOCKS
              ? `${remaining} behind tip ${tip}`
              : `scan ${scanned}/${tip} · ${fetching} fetching · ${notesFound} notes${checkpointActive ? " · saving checkpoint" : ""}`,
      });
    };

    const applyPage = async (
      blob: Uint8Array,
      end: number,
      fetching = 0,
    ): Promise<number | { reorg: number }> => {
      if (cancelled()) throw new Error("sync cancelled");
      if (blob.byteLength === 0) {
        throw new Error(`empty compact-block blob ending at ${end} (tip ${tip})`);
      }
      tick(Math.max(start, end - batch), end, fetching, "apply");
      try {
        if (ctx.transparentScan !== "compact") combinedTransparent = false;
        const s = await source.applyBlob(blob, combinedTransparent);
        if (cancelled()) throw new Error("sync cancelled");
        notesFound += s.notesFound;
        spendsFound += s.spendsFound;
        batches += 1;
        // An active write schedules a follow-up when it finishes.
        if (batches % persistEvery === 0 && !checkpointActive) startCheckpoint();
        await Promise.resolve();
        tick(s.scanned, end, fetching, "apply");
        recoveryScanned = s.scanned;
        recovery.reset();
        return s.scanned;
      } catch (e) {
        const msg = (e as Error).message || String(e);
        if (cancelled() || /scan worker restarted/i.test(msg) || /sync cancelled/i.test(msg)) {
          throw new Error("sync cancelled");
        }
        const reorgFrom = /reorg at height \d+;\s*rescan from (\d+)/i.exec(msg);
        if (reorgFrom?.[1]) return { reorg: Number(reorgFrom[1]) };
        if (/reorg/i.test(msg)) return { reorg: await source.nextHeight() };
        // Tree conflicts repeat at the same height and have their own wipe prompt.
        if (!isTreeConflictError(msg)) ctx.retireFailedSession(source);
        throw new Error(treeConflictUserMessage(msg));
      }
    };

    const openPrefetch = (from: number) =>
      createBlockPrefetch({
        start: from,
        tip,
        batch,
        prefetch,
        buffer: prefetchBuffer(prefetch),
        fetch: (h, end, signal) => combinedTransparent
          ? ctx.transport.transparentBlocks!(h, end, signal) : ctx.transport.blocks(h, end, signal),
        signal,
        cancelled,
      });
    let pipe = openPrefetch(start);
    let reorgRestarts = 0;
    try {
      // HTTP must start before GetSubtreeRoots / GetTreeState. Apply still
      // waits — cold trees cannot ingest leaves until the birthday frontier
      // is installed — but wipe+rescan must not look serial on the wire.
      await seedBirthdayTrees((proto) => {
        const fetching = pipe.inFlight();
        report({
          stage: fetching > 0 ? "downloading" : "connecting",
          heading: nearTip ? "Catching up" : fetching > 0 ? "Downloading compact blocks" : "Connecting",
          scanned: start,
          downloaded: start,
          tip,
          notesFound: 0,
          spendsFound: 0,
          percent: nearTip ? 90 : fetching > 0 ? 3 : 2,
          remainingHuman: nearTip ? `${tip - start} behind` : undefined,
          message:
            fetching > 0
              ? `fetching ${proto} subtree roots · ${fetching} block GETs prefetched`
              : `fetching ${proto} subtree roots`,
        });
      }, recovery, signal);
      if (cancelled()) throw new Error("sync cancelled");
      report({
        stage: "downloading",
        heading: nearTip ? "Catching up" : "Downloading compact blocks",
        scanned: start,
        downloaded: start,
        tip,
        notesFound: 0,
        spendsFound: 0,
        percent: nearTip ? 90 : 3,
        remainingHuman: nearTip ? `${tip - start} behind` : undefined,
        message: ctx.lwdPipe
          ? `${pipe.inFlight()} compact-block GETs on the pipe`
          : "starting compact-block download",
      });
      for (; ;) {
        if (cancelled()) throw new Error("sync cancelled");

        let job: Awaited<ReturnType<typeof pipe.next>>;
        try {
          job = await pipe.next();
        } catch (error) {
          pipe.dispose();
          try {
            await recovery.waitAfter(error);
          } catch (failure) {
            if (failure instanceof LightServerUnavailableError && !cancelled()) {
              // Keep the in-memory scan's latest safe height across a reload,
              // even when the outage happened between regular checkpoints.
              await drainCheckpoints();
              await ctx.persist().catch(() => { });
            }
            throw failure;
          }
          if (cancelled()) throw new Error("sync cancelled");
          pipe = openPrefetch(await source.nextHeight());
          continue;
        }

        if (!job) break;
        tick(job.start - 1, job.end, pipe.inFlight(), "fetch");
        const scanned = await applyPage(job.blob, job.end, pipe.inFlight());
        if (typeof scanned === "object") {
          if (++reorgRestarts > runtime.MAX_REORG_RESTARTS) {
            throw new Error(
              `chain reorganized ${reorgRestarts} times during one sync; retrying on the next sync`,
            );
          }
          const restart = runtime.reorgRestartFrom(job.start, scanned.reorg, birthday);
          pipe.dispose();
          pipe = openPrefetch(restart);
          continue;
        }
      }
    } finally {
      pipe.dispose();
    }
    await drainCheckpoints();
    if (cancelled()) throw new Error("sync cancelled");
    if (!followOn) {
      report({
        stage: "scanning",
        heading: "Saving snapshot",
        scanned: await source.scannedHeight(),
        downloaded: tip,
        tip,
        notesFound,
        spendsFound,
        percent: 97,
        message: "writing IndexedDB",
      });
    }
    await ctx.persist();
    if (cancelled()) throw new Error("sync cancelled");
    const scannedTrees = await source.scannedHeight();
    const treesLive = await source.sinsemillaLive();
    // Catch-up hashes birthday/note/tip shards via selective shard scanning. Follow-on already
    // hashed the open shard live. Do not walk sapling+ironwood+all orchard
    // from empty (the 5-minute redo) until this session actually finished a
    // far-from-tip restore.
    // Live trees need only their note/history aggregates refreshed. Do this
    // once per completed scan, not once per compact-block page during catch-up.
    if (treesLive && (notesFound > 0 || spendsFound > 0)) {
      await source.recomputePools();
      if (cancelled()) throw new Error("sync cancelled");
    }
    if (!followOn && !treesLive) {
      await paintNoteBalance({
        stage: "scanning",
        heading: "Hashing commitment trees",
        scanned: scannedTrees,
        downloaded: tip,
        tip,
        notesFound,
        spendsFound,
        percent: 98,
        message: "Hashing commitment trees",
        paintWallet: true,
      });
      let lastTick = 0;
      try {
        if (cancelled()) throw new Error("sync cancelled");
        await source.recomputePools((tick) => {
          // Progress only: the balance was painted above, and wallet reads
          // would queue behind the hashing worker.
          const now = Date.now();
          if (now - lastTick < 250) return;
          lastTick = now;
          const frac = tick.total > 0 ? tick.hashed / tick.total : 0;
          report({
            stage: "scanning",
            heading: "Hashing commitment trees",
            scanned: scannedTrees,
            downloaded: tip,
            tip,
            notesFound,
            spendsFound,
            percent: 98 + Math.min(1, frac),
            message: tick.message || "Hashing commitment trees",
          });
        });
      } catch (e) {
        const msg = (e as Error).message || String(e);
        if (/timed out/i.test(msg)) {
          console.warn("recomputePools still running in the worker after timeout; continuing");
        } else {
          const shown = treeConflictUserMessage(msg);
          report({
            stage: "scanning",
            heading: isTreeConflictError(msg) ? "Commitment tree conflict" : "Hashing failed",
            scanned: scannedTrees,
            downloaded: tip,
            tip,
            notesFound,
            spendsFound,
            percent: 98,
            message: shown,
            paintWallet: true,
          });
          throw new Error(shown);
        }
      }
    }
    if (cancelled()) throw new Error("sync cancelled");
    const scannedFinal = await source.scannedHeight();
    if (!followOn) {
      report({
        stage: "enhancing",
        heading: ctx.memoFetch === "on-demand" ? "Updating wallet" : "Reading memos",
        scanned: scannedFinal,
        downloaded: tip,
        tip,
        notesFound,
        spendsFound,
        percent: 99,
        message: ctx.memoFetch === "on-demand" ? "updating pending transactions" : "decrypting mined transaction memos",
      });
    }
    await refreshPublicData(signal);
    await refreshPostScanState(recovery, signal);
    if (cancelled()) throw new Error("sync cancelled");
    if (ctx.memoFetch === "auto") await enhanceMemos(true);
    if (cancelled()) throw new Error("sync cancelled");
    await rebroadcastPending(source);
    await ctx.persist();
    const after = await ctx.snap();
    const scanned = after.scannedHeight ?? await source.scannedHeight();
    if (cancelled()) throw new Error("sync cancelled");
    report({
      stage: "synced",
      scanned,
      downloaded: tip,
      tip,
      notesFound,
      spendsFound,
      percent: 100,
      remainingSeconds: 0,
      remainingHuman: "done",
      message: "synced",
    });
    if (cancelled()) throw new Error("sync cancelled");
    warmProvingKeyAfterSync(after);
    return autoShieldSnapshot(after);
  }

  function warmProvingKeyAfterSync(after: WalletSnapshot): void {
    // Transparent shielding creates Orchard/Ironwood outputs and uses this key
    // too, even when automatic shielding is disabled or the seed is locked.
    // Sapling-only funds are excluded: this browser prover cannot spend them.
    const spendable = after.balance.orchardAvailable + (after.balance.ironwoodAvailable ?? 0)
      + after.balance.transparentAvailable;
    const canSpend = !!(ctx.spendingSeed || runtime.runtimeState.moduleSpendingSeed) || !after.viewOnly;
    if (ctx.prewarmProvingKey && runtime.canUseProveWorker() && canSpend && spendable > 0 && !runtime.orchardProvingKeyReady()) {
      void runtime.prewarmOrchardProvingKey();
    }
  }

  async function autoShieldSnapshot(after: WalletSnapshot): Promise<WalletSnapshot> {
    if (
      ctx.opts.autoShield === true &&
      (ctx.spendingSeed || runtime.runtimeState.moduleSpendingSeed) &&
      canShield(after.balance.transparentAvailable, BigInt(ctx.opts.shieldThresholdZat ?? Number(SHIELD_THRESHOLD_ZAT)))
    ) {
      const source = ctx.session;
      const operation = ctx.sessionOperation;
      try {
        return await ctx.client.shield(ctx.opts.shieldThresholdZat ?? Number(SHIELD_THRESHOLD_ZAT));
      } catch (e) {
        // A timed-out broadcast may already have saved the pending shield.
        // The pre-shield snapshot would still show those transparent funds.
        try {
          return await ctx.snap();
        } catch {
          if (isWalletError(e) && e.code === "broadcast_failed") throw e;
          return after;
        }
      } finally {
        // An opted-in shield is a spend: consume an each-spend unlock even
        // when validation fails before proving. Never lock a replacement owner.
        if (ctx.unlockPolicy === "each-spend" && ctx.session === source &&
          ctx.sessionOperation === operation && !operation?.signal.aborted) ctx.client.lock();
      }
    }
    return after;
  }

  async function seedBirthdayTrees(
    onProto?: (proto: "sapling" | "orchard" | "ironwood") => void,
    recovery?: LightServerRecovery,
    signal?: AbortSignal,
  ): Promise<void> {
    if (!ctx.session) return;
    const source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    const scanned = await source.scannedHeight();
    const birthday = await source.birthday();
    const treesReady = await source.treesReady();
    // Roots are an optimization that can be unavailable on the first scan.
    // Refresh on a new session and periodically while running, so an existing
    // wallet can prune settled shards without resetting its scan history.
    if (treesReady && ctx.rootsChecked?.source === source && Date.now() - ctx.rootsChecked.at < 600_000) return;
    // The birthday frontier goes first: its tree sizes decide which shard the
    // roots start at. Asking from shard 0 made Zaino walk every root since the
    // pool activated (about a second each), which never finished, so the scan
    // kept every leaf and hashed all of them after the scan (minutes).
    let treeApplied = false;
    // GetTreeState would reset *_next onto already-scanned leaves.
    // Genesis needs no preceding frontier, but still needs roots for pruning.
    if (birthday > 1 && !treesReady && scanned <= birthday && ctx.transport.treeState) {
      try {
        const fetchState = () => ctx.transport.treeState!(birthday - 1, signal);
        const ts = await (recovery ? recovery.run(fetchState) : fetchState());
        ctx.assertSource(operation, source);
        await source.applyTreeState(JSON.stringify(ts));
        treeApplied = true;
      } catch (e) {
        operation.assertCurrent();
        const msg = e instanceof Error ? e.message : String(e);
        if (isTreeConflictError(msg)) throw new Error(treeConflictUserMessage(msg));
        /* scan/history still work; spendReady stays false */
      }
    }
    let rootsApplied = false;
    if (ctx.transport.subtreeRoots) {
      // Optional pruning must not stall an otherwise caught-up wallet for the
      // transport's full RPC timeout. Keep any completed prefix on a deadline;
      // user cancellation still rejects the whole operation via `signal`.
      const rootsSignal = AbortSignal.any([
        ...(signal ? [signal] : []), AbortSignal.timeout(treesReady ? 15_000 : 30_000),
      ]);
      const protos =
        ctx.network === "regtest" ? (["sapling", "orchard"] as const) : (["sapling", "orchard", "ironwood"] as const);
      // Older wasm cannot report the birthday shard; resume from the stored count.
      const starts = (await source.subtreeRootsStart()) ?? (await source.subtreeRootCounts());
      const fetched = await Promise.all(
        protos.map(async (proto) => {
          const startIndex = starts?.[proto] ?? 0;
          onProto?.(proto);
          try {
            const page = (at: number) => ctx.transport.subtreeRoots!(proto, at, rootsSignal, SUBTREE_ROOTS_PAGE);
            return { proto, startIndex, roots: await fetchSubtreeRoots(page, startIndex, signal) };
          } catch {
            // Roots only let the scan skip hashing note-free shards.
            signal?.throwIfAborted();
            return { proto, startIndex, roots: [] };
          }
        }),
      );
      for (const { proto, startIndex, roots } of fetched) {
        if (!roots.length) continue;
        ctx.assertSource(operation, source);
        await source.applySubtreeRoots(proto, JSON.stringify({ startIndex, roots }));
        rootsApplied = true;
      }
    }
    // Store roots and the birthday frontier in one snapshot. Serializing
    // twice here blocks the scan worker before its first compact block.
    if (rootsApplied || treeApplied) await ctx.persist();
    ctx.assertSource(operation, source);
    ctx.rootsChecked = { source, at: Date.now() };
  }

  async function refreshUtxos(doPersist = true): Promise<void> {
    if (ctx.rescanning) throw new WalletError("busy", "rescan is in progress");
    if (!ctx.session || !ctx.transport.utxos) return;
    const source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    const taddr = await source.transparentAddress();
    ctx.assertSource(operation, source);
    if (!taddr) return;
    const rows = await ctx.transport.utxos([taddr], 0);
    ctx.assertSource(operation, source);
    await source.applyUtxos(JSON.stringify({ utxos: rows }));
    ctx.assertSource(operation, source);
    if (doPersist) await ctx.persist();
  }

  async function refreshMempool(doPersist = true): Promise<void> {
    if (ctx.rescanning) throw new WalletError("busy", "rescan is in progress");
    if (!ctx.session || !ctx.transport.mempool) return;
    const source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    try {
      const txs = await ctx.transport.mempool();
      ctx.assertSource(operation, source);
      if (!txs.length) return;
      await source.applyMempool(JSON.stringify({ txs }));
      ctx.assertSource(operation, source);
      if (doPersist) await ctx.persist();
    } catch {
      operation.assertCurrent();
      console.warn("mempool skipped");
    }
  }

  /**
   * Resubmit our unmined sends after each sync. The same bytes are idempotent
   * ("already in the mempool"), so a broadcast whose outcome was unknown, or
   * one a refresh interrupted, reaches the network without a second send that
   * would pick other notes and could pay twice.
   */
  async function rebroadcastPending(source: ScanSession): Promise<void> {
    if (!ctx.transport.submit) return;
    const run = async () => {
      const operation = ctx.sessionOperation ?? captureWalletOperation();
      try {
        await ctx.refreshIfStale(source, operation);
      } catch (e) {
        if (isWalletError(e) && e.code === "wallet_changed") return;
        throw e;
      }
      const epoch = ctx.stateEpoch;
      let raws: string[];
      try { raws = await source.pendingRawTxs(); } catch { return; }
      if (ctx.stateEpoch !== epoch) return;
      for (const hex of raws.slice(0, 8)) {
        if (ctx.stateEpoch !== epoch) return;
        try {
          await ctx.transport.submit!(hex);
        } catch (e) {
          if (runtime.isDuplicateBroadcastError(e)) continue;
          const msg = e instanceof Error ? e.message : String(e);
          // No broadcast route or the server is down: try again next sync.
          if (/no validator rpc/i.test(msg) || isTransientLightServerError(e) || e instanceof TypeError) return;
          console.warn("a pending send was rejected on rebroadcast; it is released at expiry");
        }
      }
    };
    const locks = (globalThis as { navigator?: { locks?: LockManager } }).navigator?.locks;
    if (typeof locks?.request !== "function") {
      await run();
      return;
    }
    await locks.request("z-stack-wallet-spend", { ifAvailable: true }, (lock) => (lock ? run() : undefined));
  }

  async function refreshPublicData(signal: AbortSignal, manual = false): Promise<void> {
    const source = ctx.session;
    if (!source || (ctx.transparentScan !== "compact" && ctx.memoFetch !== "shared")) return;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    const mode = ctx.memoFetch;
    const deposits = ctx.transparentScan;
    const current = () => {
      ctx.assertSource(operation, source);
      if (mode !== ctx.memoFetch || deposits !== ctx.transparentScan) throw new Error("Privacy settings changed during retrieval");
    };
    const run = async (kind: "transparent" | "memos") => {
      try {
        const status = await syncPublicData({
          source, transport: ctx.transport, signal,
          transparent: kind === "transparent", memos: kind === "memos",
          assertCurrent: current, checkpoint: ctx.persist
        });
        current();
        if (status.transparent) ctx.transparentScanStatus = status.transparent;
        if (status.memos) ctx.sharedMemoStatus = status.memos;
      } catch (error) {
        signal.throwIfAborted();
        current();
        // A stale/forgotten wallet is not a light-server outage. Stop before
        // later phases can issue selective queries for that obsolete state.
        if (isWalletError(error) && error.code === "wallet_changed") throw error;
        if (kind === "transparent") ctx.transparentScanStatus = "unavailable";
        else ctx.sharedMemoStatus = "unavailable";
        if (manual) throw error;
        // Deposit coverage and memo availability are independent. Neither
        // failure permits a selective address/transaction fallback.
      }
    };
    if (!manual && deposits === "compact") await run("transparent");
    if (mode === "shared") await run("memos");
    if (manual && ctx.sharedMemoStatus === "unsupported") throw new Error("This server does not support shared payment notes yet");
  }

  async function refreshPostScanState(recovery?: LightServerRecovery, signal?: AbortSignal): Promise<void> {
    if (!ctx.session) return;
    const source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    const taddr = ctx.transparentScan !== "compact" && ctx.transport.utxos ? await source.transparentAddress() : undefined;
    ctx.assertSource(operation, source);
    // Network reads overlap, but wallet mutations retain the original UTXO →
    // mempool order. A failed UTXO query cannot leave a late mempool apply
    // running after sync has rejected.
    const [utxos, mempool] = await Promise.all([
      taddr && ctx.transport.utxos
        ? recovery
          ? recovery.run(() => ctx.transport.utxos!([taddr], 0, signal))
          : ctx.transport.utxos([taddr], 0, signal)
        : Promise.resolve(null),
      ctx.transport.mempool
        ? ctx.transport.mempool().catch((_e) => { console.warn("mempool skipped"); return []; })
        : Promise.resolve([]),
    ]);
    ctx.assertSource(operation, source);
    if (utxos) await source.applyUtxos(JSON.stringify({ utxos }));
    ctx.assertSource(operation, source);
    if (mempool.length) await source.applyMempool(JSON.stringify({ txs: mempool }));
    ctx.assertSource(operation, source);
  }

  async function enhanceMemos(automatic = false): Promise<void> {
    if (!ctx.session || !ctx.transport.tx) { ctx.selectiveMemoStatus = "unsupported"; return; }
    const source = ctx.session;
    const epoch = runtime.runtimeState.wasmSyncEpoch;
    const state = ctx.stateEpoch;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    const mode = ctx.memoFetch;
    const controller = new AbortController();
    ctx.memoAbort = controller;
    const signal = AbortSignal.any([operation.signal, runtime.runtimeState.syncAbort.signal, controller.signal]);
    const current = () => ctx.memoFetch === mode && (!automatic || mode === "auto") && !signal.aborted
      && source === ctx.session && epoch === runtime.runtimeState.wasmSyncEpoch && state === ctx.stateEpoch
      && (source.kind !== "worker" || source === workerScanSession());
    try {
      ctx.selectiveMemoStatus = "scanning";
      // Match manual retrieval's preflight even when deposit scanning is off.
      // A failed save must not let an obsolete tab keep requesting owned IDs.
      if (automatic) {
        await ctx.refreshIfStale(source, operation);
        if (!current()) return;
      }
      let need = await source.memoEnhancementTxids(500);
      const legacy = need === null;
      if (need === null) {
        // Compatibility with older generated artifacts. New builds persist the
        // completion flag independently of memo text, including successful blanks.
        const rows = JSON.parse(await source.history(80)) as HistoryEntry[];
        need = rows.filter((e) => !(e.memos && e.memos.length > 0)
          && ((e.receivedNoteCount ?? 0) + (e.sentNoteCount ?? 0) > 0 || e.isShielding))
          .slice(0, 40).map((e) => e.txid);
      }
      let n = 0;
      let failed = false;
      const conc = 8;
      for (let i = 0; i < need.length && current(); i += conc) {
        const chunk = need.slice(i, i + conc);
        let outages = 0;
        const hexes = await Promise.all(
          chunk.map((txid) =>
            ctx.transport.tx!(txid, signal).catch((error) => {
              if (isTransientLightServerError(error)) outages++;
              return null;
            }),
          ),
        );
        for (const hex of hexes) {
          if (!current()) return;
          if (!hex) { failed = true; continue; }
          try {
            await source.enhanceRawTx(hex);
            n += 1;
          } catch {
            failed = true; // Keep failed entries in Rust's durable enhancement queue.
          }
        }
        // Persist progress in bounded groups, including successful blank memos.
        // Storage failures must escape, not masquerade as a network retry.
        if (n >= 40 && current()) { await ctx.persist(); n = 0; }
        // A down server must not consume 500 sequential request deadlines.
        // Permanent per-transaction errors still allow later entries to run.
        if (outages === chunk.length) break;
      }
      if (!current()) return;
      // Also retry a previously failed save when every entry completed in memory.
      await ctx.persist();
      const pending = legacy ? need.length >= 40 : (await source.memoEnhancementTxids(1))?.length;
      if (current()) ctx.selectiveMemoStatus = pending ? (failed || need.length < (legacy ? 40 : 500) ? "unavailable" : "scanning") : failed ? "unavailable" : "complete";
    } finally {
      if (ctx.memoAbort === controller) ctx.memoAbort = null;
    }
  }

  function fetchMemos(): Promise<WalletSnapshot> {
    if (ctx.rescanning) return Promise.reject(new WalletError("busy", "rescan is in progress"));
    ctx.currentSession();
    if (ctx.memoLock?.epoch === runtime.runtimeState.wasmSyncEpoch) return ctx.memoLock.promise;
    const operation = captureWalletOperation();
    const epoch = runtime.runtimeState.wasmSyncEpoch;
    const preceding = ctx.syncLock?.epoch === epoch ? ctx.syncLock.promise : undefined;
    const promise = (async () => {
      // A manual enhancement must not replace an active scan's snapshot. Failed
      // sync does not prevent a user from reading memos already in local history.
      await preceding?.catch(() => { });
      await operation.ready();
      if (epoch !== runtime.runtimeState.wasmSyncEpoch) throw new Error("memo fetch cancelled");
      if (!ctx.currentSession() && !(await ctx.loadIfNeeded())) throw new Error("no wasm wallet");
      const source = ctx.session!;
      ctx.assertSource(operation, source);
      // Verify the durable generation/revision before sending any owned txids.
      await ctx.refreshIfStale(source, operation);
      ctx.assertSource(operation, source);
      if (epoch !== runtime.runtimeState.wasmSyncEpoch) throw new Error("memo fetch cancelled");
      if (ctx.memoFetch === "shared") await refreshPublicData(operation.signal, true);
      else await enhanceMemos();
      ctx.assertSource(operation, source);
      if (epoch !== runtime.runtimeState.wasmSyncEpoch) throw new Error("memo fetch cancelled");
      if (walletStorageAvailable() && await readWalletGeneration(operation.signal) !== ctx.storageGeneration) {
        throw new WalletError("wallet_changed", "the saved wallet was replaced or forgotten in another tab; reload it");
      }
      ctx.assertSource(operation, source);
      return ctx.snap();
    })().catch((e) => { throw WalletError.fromUnknown(e); });
    const entry = { epoch, promise };
    ctx.memoLock = entry;
    const release = () => { if (ctx.memoLock === entry) ctx.memoLock = null; };
    void promise.then(release, release);
    return promise;
  }
  return { report, paintNoteBalance, runSync, refreshUtxos, refreshMempool, fetchMemos };
}
