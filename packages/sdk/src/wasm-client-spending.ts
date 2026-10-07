import {
  isWalletError,
  WalletError
} from "@z-stack/core";
import type { FeeEstimate, MaxSend } from "./engine";
import { balanceEvent } from "./events";
import {
  hardwareHex,
  type HardwareSendOptions,
  type HardwareSignStage,
  type LedgerSigningPlan
} from "./hardware";
import {
  interruptScanWorkerSession,
  treeConflictUserMessage,
  type ScanSession
} from "./scan-host";
import {
  readSavedSnapshotRecord
} from "./snapshot-storage";
import { captureWalletOperation, type WalletOperation } from "./wallet-lifecycle";
import { abortable, walletStorageAvailable } from "./wallet-storage";
import type { WasmClientContext } from './wasm-client-context';
import * as runtime from './wasm-client-runtime';

/** Software and hardware spend reservations, proving and broadcast. */
export function createSpendingController(ctx: Pick<WasmClientContext, "sessionOperation" | "currentSession" | "loadIfNeeded" | "session" | "snap" | "assertSource" | "disposing" | "refreshIfStale" | "transport" | "stateEpoch" | "savedKeySeen" | "persist" | "beginReservationDrain" | "adoptSaved" | "bus" | "unlockPolicy" | "spendingSeed" | "disposeWaits" | "storageGeneration" | "bindLocal" | "applyLoaded" | "copyLegacy" | "refreshMempool">) {

  async function transparentSendSource(): Promise<{ source: ScanSession; operation: WalletOperation; supported: boolean } | null> {
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    if (!ctx.currentSession() && !(await ctx.loadIfNeeded())) return null;
    operation.assertCurrent();
    const source = ctx.session!;
    const wallet = await ctx.snap();
    ctx.assertSource(operation, source);
    if (wallet.hardware) return { source, operation, supported: false };
    const supported = await source.supportsTransparentSend?.() ?? false;
    ctx.assertSource(operation, source);
    return { source, operation, supported };
  }

  function proveAndSubmit(
    kind: "send" | "sendTransparent" | "shield",
    seed: string,
    args: { to?: string; amountZec?: string; memo?: string; maxFeeZat?: string; thresholdZat?: number },
    beforeBroadcast?: () => boolean,
  ): Promise<string | undefined> {
    return runtime.withOriginSpendLock(() => proveAndSubmitHere(kind, seed, args, beforeBroadcast));
  }

  async function proveAndSubmitHere(
    kind: "send" | "sendTransparent" | "shield",
    seed: string,
    args: { to?: string; amountZec?: string; memo?: string; maxFeeZat?: string; thresholdZat?: number },
    beforeBroadcast?: () => boolean,
  ): Promise<string | undefined> {
    if (!ctx.session) throw new Error("no wasm wallet");
    const source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    ctx.assertSource(operation, source);
    const assertBroadcastAllowed = () => {
      if (ctx.disposing) throw new DOMException("wallet closing before broadcast", "AbortError");
      if (beforeBroadcast && beforeBroadcast() !== true) {
        throw new DOMException("swap deposit expired or was cancelled before broadcast", "AbortError");
      }
    };
    assertBroadcastAllowed();
    // Any in-flight spend blocks this one. Comparing to this call's signal
    // missed a second send that captured its own operation.
    if (runtime.runtimeState.spendingOperation) throw new WalletError("busy", "a wallet spend is already in progress");
    runtime.runtimeState.spendingOperation = operation.signal;
    try {
      await ctx.refreshIfStale(source, operation);
      // The transaction expires 40 blocks past the scanned height. Far behind the
      // tip, the node would reject it as expired while its inputs stay reserved.
      const [scanned, tip] = await Promise.all([source.scannedHeight(), ctx.transport.tip()]);
      ctx.assertSource(operation, source);
      if (tip - scanned > runtime.SPEND_MAX_LAG_BLOCKS) {
        throw new WalletError(
          "sync_required",
          `wallet is ${tip - scanned} blocks behind the chain tip; sync before sending`,
        );
      }
    } catch (e) {
      if (runtime.runtimeState.spendingOperation === operation.signal) runtime.runtimeState.spendingOperation = null;
      throw e;
    }
    let backup: Uint8Array | undefined;
    let published = false;
    let proofInterrupted = false;
    let broadcastStarted = false;
    let sentTxid: string | undefined;
    let finishDrain: ((error?: unknown) => void) | undefined;
    let rollbackFailure: unknown;
    const stateAtSend = ctx.stateEpoch;
    const baseAtSend = ctx.savedKeySeen;
    const sameLineage = () => ctx.stateEpoch === stateAtSend;
    /** Undo only this send, so a sync or new address that landed meanwhile survives. */
    const undoSend = async () => {
      for (let attempt = 0; attempt < 4; attempt++) {
        // A wallet_changed reload already holds the other tab's snapshot.
        // Restoring this tab's pre-send backup on top of it would erase their save.
        const adopted = ctx.stateEpoch !== stateAtSend;
        if (sentTxid) await source.hardware("rollback", { txid: sentTxid });
        else if (backup && !adopted) await source.fromSnapshot(backup);
        else return;
        ctx.assertSource(operation, source);
        const epoch = ctx.stateEpoch;
        const base = ctx.savedKeySeen;
        try {
          await ctx.persist({ epoch, baseKey: base });
        } catch (error) {
          // Another tab saved over the rollback. The reload kept their copy
          // of this transaction; abandon it again on that copy.
          if (!isWalletError(error) || error.code !== "wallet_changed" || attempt === 3) throw error;
          continue;
        }
        // A queued save can commit the snapshot adopted from the other tab,
        // which still contains this transaction. That persist resolves, so
        // success is not enough: the epoch must be the one we rolled back.
        if (ctx.stateEpoch === epoch) return;
      }
      throw new WalletError("wallet_changed", "the wallet was saved by another tab");
    };
    const noSubmit = "this block transport cannot broadcast (use the loopback /lwd proxy)";
    try {
      backup = await source.toSnapshot();
      ctx.assertSource(operation, source);
      assertBroadcastAllowed();
      let hex: string;
      let txid: string | undefined;
      if (source.prove) {
        // The multicore scan worker proves on its own wallet, whose trees are
        // built, with all of its threads. The wallet records the pending
        // transaction itself, so any failure before broadcast restores `backup`.
        if (!ctx.transport.submit) throw new Error(noSubmit);
        if (!sameLineage()) throw new WalletError("wallet_changed", "the wallet was saved by another tab");
        finishDrain = ctx.beginReservationDrain();
        published = true;
        ({ hex, txid } = await interruptibleProof(source, operation, () => source.prove!(kind, seed, args), () => {
          proofInterrupted = true;
          // RPCs behind an unfinished proof cannot serialize its reservation.
          // Retirement discards it; the durable wallet remains unchanged.
          published = false;
        }));
        sentTxid = txid;
        ctx.assertSource(operation, source);
        if (!hex) throw new Error("prove returned no transaction");
        if (!sameLineage()) {
          await ctx.adoptSaved(source, operation);
          throw new WalletError("wallet_changed", "the wallet was saved by another tab");
        }
      } else {
        let proved: runtime.ProveResult;
        const worker = runtime.getProveWorker();
        if (worker) {
          proved = await interruptibleProof(source, operation, () => runtime.proveInWorker({ snapshot: backup!, kind, mnemonic: seed, ...args }));
        } else {
          const handle = runtime.requireBindings().WasmWallet.fromSnapshot(backup);
          try {
            const result = kind === "send"
              ? handle.proveSend(seed, args.to ?? "", args.amountZec ?? "0", args.memo)
              : kind === "sendTransparent"
                ? handle.proveTransparentSend?.(seed, args.to ?? "", args.amountZec ?? "0", args.maxFeeZat) ?? (() => { throw new Error("this wasm build cannot prove transparent swap outputs"); })()
                : handle.proveShield(seed, args.thresholdZat ?? 100_000);
            const parsed = JSON.parse(result) as { hex: string; txid?: string };
            proved = { hex: parsed.hex, ...(parsed.txid ? { txid: parsed.txid } : {}), snapshot: handle.toSnapshot() };
          } finally { handle.free?.(); }
        }
        ctx.assertSource(operation, source);
        if (!proved.hex || !proved.snapshot) throw new Error("prove returned no transaction or wallet snapshot");
        if (!ctx.transport.submit) throw new Error(noSubmit);
        if (!sameLineage()) throw new WalletError("wallet_changed", "the wallet was saved by another tab");
        // Reserve inputs durably before broadcast. Once the request is sent a lost
        // response cannot establish rejection, so its pending transaction survives.
        finishDrain = ctx.beginReservationDrain();
        await source.fromSnapshot(proved.snapshot);
        ctx.assertSource(operation, source);
        published = true;
        hex = proved.hex;
        txid = proved.txid;
        sentTxid = txid;
      }
      assertBroadcastAllowed();
      if (!sentTxid) throw new WalletError("unknown", "The engine did not return a transaction ID; nothing was broadcast.");
      const reservationGeneration = runtime.runtimeState.persistGen;
      if (!sameLineage()) throw new WalletError("wallet_changed", "the wallet was saved by another tab");
      await ctx.persist({ epoch: stateAtSend, baseKey: baseAtSend });
      ctx.assertSource(operation, source);
      if (reservationGeneration !== runtime.runtimeState.persistGen) throw new Error("saving the pending transaction was cancelled; transaction was not broadcast");
      // A conflicting save reloaded this tab. The proved bytes belong to the
      // wallet that was replaced, so they must not be broadcast.
      if (!sameLineage()) throw new WalletError("wallet_changed", "the wallet was saved by another tab");
      assertBroadcastAllowed();
      ctx.bus.emit("broadcast", { kind: kind === "sendTransparent" ? "send" : kind, ...(sentTxid ? { txid: sentTxid } : {}) });
      ctx.assertSource(operation, source);
      assertBroadcastAllowed();
      broadcastStarted = true;
      finishDrain?.();
      // Stop waiting on close/replacement without claiming the already-started
      // request was cancelled. The pending transaction and known ID survive.
      const submitted = await abortable(operation.signal, () => ctx.transport.submit!(hex));
      ctx.assertSource(operation, source);
      return txid ?? (/^[0-9a-f]{64}$/i.test(submitted) ? submitted.toLowerCase() : undefined);
    } catch (e) {
      if (proofInterrupted) throw new DOMException("wallet proof cancelled before broadcast", "AbortError");
      if (operation.signal.aborted || source !== ctx.session) {
        if (broadcastStarted) throw new WalletError("broadcast_failed",
          "Wallet changed after submission started. Check this transaction before sending another payment.", e, { txid: sentTxid });
        throw e;
      }
      // After wallet_changed this tab already holds the saved state, which
      // never had this send.
      const reloaded = isWalletError(e) && e.code === "wallet_changed";
      if (!broadcastStarted && published) {
        try {
          await undoSend();
        } catch (undoError) {
          if (!reloaded) { rollbackFailure = undoError; throw undoError; }
        }
      }
      const message = e instanceof Error ? e.message : String(e);
      const verdict = broadcastStarted ? runtime.submitVerdict(e) : null;
      if (verdict === "accepted") return sentTxid;
      if (verdict === "rejected") {
        // The node refused this transaction: undo it now (its notes are free
        // again) rather than keep resending it until it expires.
        await undoSend();
        throw new WalletError("broadcast_rejected", `broadcast rejected: ${message}`, e);
      }
      if (broadcastStarted) {
        // The txid lets the app show this send as pending (it is saved and
        // resent) instead of offering a retry that could pay twice.
        throw new WalletError(
          "broadcast_failed",
          `broadcast outcome unknown; check the transaction before sending another payment: ${message}`,
          e,
          { txid: sentTxid },
        );
      }
      if (reloaded) throw e;
      if (e instanceof DOMException && e.name === "AbortError") throw e;
      throw new Error(treeConflictUserMessage(message));
    } finally {
      finishDrain?.(rollbackFailure);
      if (runtime.runtimeState.spendingOperation === operation.signal) runtime.runtimeState.spendingOperation = null;
      if (!operation.signal.aborted && ctx.unlockPolicy === "each-spend") {
        ctx.spendingSeed = null;
        runtime.runtimeState.moduleSpendingSeed = null;
        runtime.saveSessionSeed(null);
      }
    }
  }

  /** Interrupt the exact actor on close/replacement, without touching a successor. */
  async function interruptibleProof<T>(source: ScanSession, operation: WalletOperation,
    start: () => Promise<T>, onScanInterrupted?: () => void, signal?: AbortSignal): Promise<T> {
    const signals = [ctx.disposeWaits.signal, operation.signal, ...(signal ? [signal] : [])];
    signals.forEach(value => value.throwIfAborted());
    const statelessWorker = source.prove ? null : runtime.getProveWorker();
    const interrupt = () => {
      if (source.prove) {
        if (!interruptScanWorkerSession(source)) return;
        if (runtime.runtimeState.liveWorkerWallet?.session === source) runtime.runtimeState.liveWorkerWallet = null;
        onScanInterrupted?.();
      } else if (statelessWorker && runtime.runtimeState.proveWorker === statelessWorker) {
        runtime.proveWorkerSessions.get(statelessWorker)?.fail(new DOMException("wallet proof cancelled before broadcast", "AbortError"));
      }
    };
    signals.forEach(value => value.addEventListener("abort", interrupt, { once: true }));
    try { return await start(); }
    finally { signals.forEach(value => value.removeEventListener("abort", interrupt)); }
  }

  /** The killed worker cannot answer rollback RPCs. Recover its latest durable state locally. */
  async function recoverInterruptedSession(source: ScanSession, operation: WalletOperation): Promise<ScanSession> {
    operation.assertCurrent();
    if (source !== ctx.session) throw new DOMException("wallet changed during proof cancellation", "AbortError");
    if (!walletStorageAvailable()) throw new WalletError("wallet_db", "Local wallet storage is unavailable during proof cancellation.");
    const record = await readSavedSnapshotRecord(operation.signal);
    operation.assertCurrent();
    if (!record || record.generation !== ctx.storageGeneration) {
      throw new WalletError("wallet_changed", "The saved wallet changed during proof cancellation; reopen it.");
    }
    const next = ctx.bindLocal(runtime.requireBindings().WasmWallet.fromSnapshot(record.bytes), operation, record.generation);
    ctx.applyLoaded(record, ctx.copyLegacy(record));
    ctx.stateEpoch += 1;
    return next;
  }

  /** Stateless PCZT proving where it is fastest: the multicore scan worker, the prove worker, or here. */
  async function proveHardwarePczt(source: ScanSession, pczt: Uint8Array): Promise<Uint8Array> {
    if (!source.prove && runtime.getProveWorker()) {
      const copy = pczt.slice();
      const data = await runtime.workerCall({ kind: "hardwareProve", pczt: copy.buffer }, [copy.buffer]);
      const out = data.pczt as ArrayBuffer | Uint8Array;
      return out instanceof Uint8Array ? out : new Uint8Array(out);
    }
    const { bytes } = await source.hardware("prove", { pczt });
    if (!bytes) throw new Error("hardware prove returned no PCZT");
    return bytes;
  }

  /**
   * Hardware send: PCZT → (prove ∥ device review) → apply signatures →
   * finalize → persist the pending transaction → broadcast. Notes are
   * reserved in memory while the device signs. Any failure before broadcast
   * undoes only this send (its reservations and, once finalized, its pending
   * transaction) and saves that, so sync and address changes made while the
   * device was busy are kept.
   */
  function hardwareSubmit(
    args: { to: string; amountZec: string; memo?: string },
    opts: HardwareSendOptions,
  ): Promise<string | undefined> {
    return runtime.withOriginSpendLock(() => hardwareSubmitHere(args, opts));
  }

  async function hardwareSubmitHere(
    args: { to: string; amountZec: string; memo?: string },
    { signer, onStage: userOnStage, signal, beforeBroadcast }: HardwareSendOptions,
  ): Promise<string | undefined> {
    if (!ctx.session) throw new Error("no wasm wallet");
    let source = ctx.session;
    const operation = ctx.sessionOperation ?? captureWalletOperation();
    ctx.assertSource(operation, source);
    const waitForDevice = async <T>(start: () => Promise<T>): Promise<T> => {
      const waiting = new AbortController();
      const signals = [ctx.disposeWaits.signal, operation.signal, ...(signal ? [signal] : [])];
      const listeners = signals.map(source => {
        const forward = () => waiting.abort(source.reason);
        source.addEventListener("abort", forward, { once: true });
        if (source.aborted) forward();
        return () => source.removeEventListener("abort", forward);
      });
      try { return await abortable(waiting.signal, start); }
      finally { listeners.forEach(remove => remove()); }
    };
    const assertReviewCurrent = () => {
      if (ctx.disposing) throw new DOMException("wallet closing before broadcast", "AbortError");
      signal?.throwIfAborted();
      if (beforeBroadcast && beforeBroadcast() !== true) throw new DOMException("Send review expired or was cancelled before broadcast", "AbortError");
    };
    assertReviewCurrent();
    if (runtime.runtimeState.spendingOperation) throw new WalletError("busy", "a wallet spend is already in progress");
    runtime.runtimeState.spendingOperation = operation.signal;
    // A progress callback must not be able to fail (or half-finish) a send.
    const onStage = (stage: HardwareSignStage) => {
      if (ctx.disposing || signal?.aborted || operation.signal.aborted || source !== ctx.session) return;
      try {
        userOnStage?.(stage);
      } catch {
        console.warn("hardware onStage callback threw");
      }
    };
    let proofInterrupted = false;
    let removeReservationInterrupt: (() => void) | undefined;
    let reserved = false;
    let finalizedTxid: string | undefined;
    let broadcastStarted = false;
    let stateAtHardware = 0;
    let finishDrain: ((error?: unknown) => void) | undefined;
    let rollbackFailure: unknown;
    try {
      await ctx.refreshIfStale(source, operation);
      stateAtHardware = ctx.stateEpoch;
      const baseAtHardware = ctx.savedKeySeen;
      if (!ctx.transport.submit) throw new Error("this block transport cannot broadcast (use the loopback /lwd proxy)");
      const [scanned, tip] = await Promise.all([source.scannedHeight(), ctx.transport.tip()]);
      ctx.assertSource(operation, source);
      if (tip - scanned > runtime.SPEND_MAX_LAG_BLOCKS) {
        throw new WalletError("sync_required", `wallet is ${tip - scanned} blocks behind the chain tip; sync before sending`);
      }
      onStage("connecting");
      // Check the device before reserving anything.
      const appVersion = signer.device === "ledger" ? await waitForDevice(() => signer.appVersion()) : undefined;
      ctx.assertSource(operation, source);
      assertReviewCurrent();
      finishDrain = ctx.beginReservationDrain();
      const reservationSource = source;
      const reservationSignals = [ctx.disposeWaits.signal, operation.signal, ...(signal ? [signal] : [])];
      const interruptReservation = () => {
        if (broadcastStarted || reservationSource.kind !== "worker" || !interruptScanWorkerSession(reservationSource)) return;
        proofInterrupted = true;
        if (runtime.runtimeState.liveWorkerWallet?.session === reservationSource) runtime.runtimeState.liveWorkerWallet = null;
      };
      reservationSignals.forEach(value => value.addEventListener("abort", interruptReservation, { once: true }));
      removeReservationInterrupt = () => reservationSignals.forEach(value => value.removeEventListener("abort", interruptReservation));
      const created = await source.hardware("createSend", args);
      reserved = true;
      ctx.assertSource(operation, source);
      const pczt = created.bytes;
      if (!pczt) throw new Error("hardware send returned no PCZT");
      // The device only ever sees what it needs: a Ledger gets APDUs built from
      // the PCZT, a Keystone a redacted copy.
      const request = signer.device === "ledger"
        ? await source.hardware("ledgerPlan", { pczt, appVersion: appVersion ?? "" })
        : await source.hardware("signerCopy", { pczt, copy: "full" });
      ctx.assertSource(operation, source);
      // Prove while the user reviews on the device.
      const proving = interruptibleProof(source, operation, () => proveHardwarePczt(source, pczt),
        () => { proofInterrupted = true; }, signal);
      proving.catch(() => undefined);
      let signed: Uint8Array | undefined;
      if (signer.device === "ledger") {
        const plan = JSON.parse(request.json ?? "{}") as LedgerSigningPlan;
        const responses = await waitForDevice(() => signer.exchange(plan, onStage));
        ctx.assertSource(operation, source);
        onStage("proving");
        const proved = await proving;
        ctx.assertSource(operation, source);
        signed = (await source.hardware("ledgerApply", {
          pczt: proved,
          json: JSON.stringify(responses.map((r) => hardwareHex.hex(r))),
        })).bytes;
      } else {
        if (!request.bytes) throw new Error("hardware signer copy is missing");
        onStage("reviewing");
        const deviceSigned = await waitForDevice(() => signer.sign(request.bytes!));
        ctx.assertSource(operation, source);
        onStage("proving");
        const proved = await proving;
        ctx.assertSource(operation, source);
        signed = (await source.hardware("combine", { pczt: proved, signed: deviceSigned })).bytes;
      }
      ctx.assertSource(operation, source);
      if (!signed) throw new Error("applying the device signatures returned no PCZT");
      if (ctx.stateEpoch !== stateAtHardware) {
        throw new WalletError("wallet_changed", "the wallet was saved by another tab");
      }
      const done = JSON.parse((await source.hardware("finalize", { pczt: signed })).json ?? "{}") as {
        txid?: string;
        hex?: string;
      };
      finalizedTxid = done.txid;
      ctx.assertSource(operation, source);
      if (!done.hex) throw new Error("finalize returned no transaction");
      if (!finalizedTxid) throw new WalletError("unknown", "The engine did not return a transaction ID; nothing was broadcast.");
      if (ctx.stateEpoch !== stateAtHardware) {
        throw new WalletError("wallet_changed", "the wallet was saved by another tab");
      }
      // Reserve inputs durably before broadcast (see proveAndSubmit).
      const reservationGeneration = runtime.runtimeState.persistGen;
      await ctx.persist({ epoch: stateAtHardware, baseKey: baseAtHardware });
      ctx.assertSource(operation, source);
      if (reservationGeneration !== runtime.runtimeState.persistGen) throw new Error("saving the pending transaction was cancelled; transaction was not broadcast");
      if (ctx.stateEpoch !== stateAtHardware) {
        throw new WalletError("wallet_changed", "the wallet was saved by another tab");
      }
      onStage("broadcasting");
      ctx.assertSource(operation, source);
      assertReviewCurrent();
      ctx.bus.emit("broadcast", { kind: "send", txid: finalizedTxid });
      ctx.assertSource(operation, source);
      assertReviewCurrent();
      broadcastStarted = true;
      finishDrain?.();
      const submitted = await abortable(operation.signal, () => ctx.transport.submit!(done.hex!));
      ctx.assertSource(operation, source);
      return done.txid ?? (/^[0-9a-f]{64}$/i.test(submitted) ? submitted.toLowerCase() : undefined);
    } catch (e) {
      if (operation.signal.aborted || source !== ctx.session) {
        if (broadcastStarted) throw new WalletError("broadcast_failed",
          "Wallet changed after submission started. Check this transaction before sending another payment.", e, { txid: finalizedTxid });
        throw e;
      }
      if (proofInterrupted && reserved && !broadcastStarted) {
        try { source = await recoverInterruptedSession(source, operation); }
        catch (error) { rollbackFailure = error; throw error; }
      }
      const reloaded = isWalletError(e) && e.code === "wallet_changed";
      if (!broadcastStarted && reserved && !reloaded) {
        // Durable too: a background save may already hold the reservation
        // or the finalized transaction.
        try {
          for (let attempt = 0; attempt < 4; attempt++) {
            await source.hardware("rollback", finalizedTxid ? { txid: finalizedTxid } : {});
            ctx.assertSource(operation, source);
            const epoch = ctx.stateEpoch;
            try {
              await ctx.persist({ epoch, baseKey: ctx.savedKeySeen });
            } catch (error) {
              if (!isWalletError(error) || error.code !== "wallet_changed" || attempt === 3) throw error;
              continue;
            }
            if (ctx.stateEpoch === epoch) break;
          }
        } catch (error) { rollbackFailure = error; throw error; }
      }
      if (proofInterrupted && !broadcastStarted) throw new DOMException("wallet proof cancelled before broadcast", "AbortError");
      const message = e instanceof Error ? e.message : String(e);
      const verdict = broadcastStarted ? runtime.submitVerdict(e) : null;
      if (verdict === "accepted") return finalizedTxid;
      if (verdict === "rejected" && finalizedTxid) {
        for (let attempt = 0; attempt < 4; attempt++) {
          await source.hardware("rollback", { txid: finalizedTxid });
          ctx.assertSource(operation, source);
          const epoch = ctx.stateEpoch;
          const base = ctx.savedKeySeen;
          try {
            await ctx.persist({ epoch, baseKey: base });
          } catch (error) {
            if (!isWalletError(error) || error.code !== "wallet_changed" || attempt === 3) throw error;
            continue;
          }
          if (ctx.stateEpoch === epoch) break;
        }
        throw new WalletError("broadcast_rejected", `broadcast rejected: ${message}`, e);
      }
      if (broadcastStarted) {
        throw new WalletError(
          "broadcast_failed",
          `broadcast outcome unknown; transaction remains pending. Sync before retrying: ${message}`,
          e,
          { txid: finalizedTxid },
        );
      }
      throw WalletError.fromUnknown(e);
    } finally {
      removeReservationInterrupt?.();
      finishDrain?.(rollbackFailure);
      if (runtime.runtimeState.spendingOperation === operation.signal) runtime.runtimeState.spendingOperation = null;
    }
  }

  function requireSeed(action: string): string {
    const seed = ctx.spendingSeed || runtime.runtimeState.moduleSpendingSeed;
    if (!seed) {
      throw new Error(`re-enter spending seed to ${action} (mnemonic is not stored in the snapshot)`);
    }
    return seed;
  }

  /** Once submitted, even a failed UI refresh must retain the receipt. */
  async function finishSpend(txid: string | undefined, operation: WalletOperation, source: ScanSession) {
    try {
      ctx.assertSource(operation, source);
      await ctx.refreshMempool();
      ctx.assertSource(operation, source);
      const wallet = await ctx.snap();
      ctx.assertSource(operation, source);
      const balance = balanceEvent(wallet);
      if (balance) ctx.bus.emit("balance", balance);
      return txid ? { ...wallet, txid } : wallet;
    } catch (error) {
      throw new WalletError("broadcast_failed", "Submission finished but the wallet could not refresh. Check this transaction before sending another payment.", error, { txid });
    }
  }

  async function parseFeeJson(raw: string): Promise<FeeEstimate> {
    const v = JSON.parse(raw) as FeeEstimate;
    if (typeof v.feeZat !== "number") throw new Error("invalid fee estimate");
    return v;
  }

  async function parseMaxJson(raw: string): Promise<MaxSend> {
    const v = JSON.parse(raw) as MaxSend;
    if (typeof v.maxSendZat !== "number") throw new Error("invalid max send");
    return v;
  }
  return { transparentSendSource, proveAndSubmit, proveAndSubmitHere, hardwareSubmit, requireSeed, finishSpend, parseFeeJson, parseMaxJson };
}
