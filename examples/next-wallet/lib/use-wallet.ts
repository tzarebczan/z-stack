"use client";
import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { createWallet, WalletError, walletErrorMessage, type HistoryEntry,
  type Wallet, type WalletSnapshot } from "@z-stack/sdk";
import { connection } from "./connection";
import { walletLifetime } from "./lifetime";
import { pendingRecovery } from "./recovery-memory";
import { reviewSend, recheckReview, refreshReceipt, type SendDraft, type SendReview, type SendReceipt } from "./send";

const acquire = walletLifetime(() => createWallet({ ...connection, autoSync: false,
  autoShield: false, memoFetch: "on-demand", unlockPolicy: "each-spend", threads: 2 }));
const safeError = (error: unknown) => error instanceof WalletError
  ? walletErrorMessage(error.code) : "Could not complete this action.";

export function useWallet() {
  const owner = useRef<Wallet | undefined>(undefined);
  const running = useRef(false);
  const sendOperation = useRef<AbortController | undefined>(undefined);
  const [receipt, setReceipt] = useState<SendReceipt>();
  const [canCancelPayment, setCanCancelPayment] = useState(false);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState("");
  const [status, setStatus] = useState("Opening local wallet…");
  const [snapshot, setSnapshot] = useState<WalletSnapshot>();
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const pending = useSyncExternalStore(pendingRecovery.subscribe, pendingRecovery.snapshot, () => undefined);
  const [pageVisible, setPageVisible] = useState(true);
  const phrase = pageVisible && pending?.address === snapshot?.unifiedAddress ? pending?.phrase ?? "" : "";
  const [spending, setSpending] = useState(false);
  const [runtime, setRuntime] = useState("Scan engine starts on sync");
  const [progress, setProgress] = useState<number>();

  useEffect(() => {
    if (!phrase && busy !== "Creating") return;
    const beforeUnload = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };
    const beforeLink = (event: MouseEvent) => {
      if (!(event.target instanceof Element) || !event.target.closest("a[href]")) return;
      event.preventDefault(); event.stopPropagation();
      setStatus(phrase ? "Save your recovery phrase before leaving." : "Wait for wallet creation to finish.");
    };
    window.addEventListener("beforeunload", beforeUnload);
    document.addEventListener("click", beforeLink, true);
    return () => {
      window.removeEventListener("beforeunload", beforeUnload);
      document.removeEventListener("click", beforeLink, true);
    };
  }, [phrase, busy]);

  function updateRuntime(wallet: Wallet) {
    setRuntime(wallet.runtime.scanWorker ? (wallet.runtime.mode === "multi-thread" ? `${wallet.runtime.threads} threads` : "Single thread") : "Scan engine starts on sync");
    setSpending(wallet.hasSpendingSeed());
  }
  async function refresh(wallet: Wallet, value: WalletSnapshot) {
    if (owner.current !== wallet) return;
    setSnapshot(value); updateRuntime(wallet);
    // A failed optional read must never undo creation or hide its one-time phrase.
    const entries = await wallet.history(20);
    if (owner.current === wallet) setHistory(entries);
  }
  useEffect(() => {
    setReady(false); setBusy(""); setSpending(false); setStatus("Opening local wallet…");
    const lease = acquire();
    let disposed = false;
    let off: (() => void) | undefined;
    let offBroadcast: (() => void) | undefined;
    const onHide = () => {
      setPageVisible(false);
      sendOperation.current?.abort();
      owner.current?.lock(); owner.current?.cancelSync();
      setSpending(false);
    };
    const onShow = () => setPageVisible(true);
    window.addEventListener("pagehide", onHide);
    window.addEventListener("pageshow", onShow);
    void lease.ready.then(async wallet => {
      if (disposed || !wallet) return;
      owner.current = wallet;
      off = wallet.on("sync", value => {
        if (!disposed) setProgress(value.stage === "synced" ? 100 : value.percent);
      });
      offBroadcast = wallet.on("broadcast", () => {
        if (!disposed && sendOperation.current) { setCanCancelPayment(false); setStatus("Submitting payment…"); }
      });
      const saved = await wallet.load();
      if (disposed) return;
      updateRuntime(wallet);
      setStatus(saved ? pendingRecovery.snapshot()?.address === saved.unifiedAddress
        ? "Save your recovery phrase." : "Wallet opened. Spending is locked." : "Ready to create or restore.");
      setReady(true);
      if (saved) await refresh(wallet, saved);
    }).catch(error => { if (!disposed) setStatus(safeError(error)); });
    return () => {
      disposed = true; sendOperation.current?.abort(); owner.current = undefined; off?.(); offBroadcast?.();
      window.removeEventListener("pagehide", onHide);
      window.removeEventListener("pageshow", onShow); lease.release();
    };
  }, []);

  async function run<T>(label: string, action: (wallet: Wallet) => Promise<T>): Promise<T | undefined> {
    const wallet = owner.current;
    if (!wallet || !ready || running.current) return;
    running.current = true; setBusy(label);
    try { return await action(wallet); }
    catch (error) { if (owner.current === wallet) setStatus(safeError(error)); }
    finally {
      running.current = false;
      if (owner.current === wallet) { updateRuntime(wallet); setBusy(""); setProgress(undefined); }
    }
  }
  async function baseAction<T>(action: (wallet: Wallet) => Promise<T>): Promise<T> {
    const wallet = owner.current;
    if (!wallet || !ready || running.current || phrase) throw new Error("Another wallet action is in progress.");
    running.current = true; setBusy("Base");
    try { return await action(wallet); }
    finally {
      wallet.lock(); running.current = false;
      if (owner.current === wallet) { updateRuntime(wallet); setBusy(""); }
    }
  }
  let serverName = "Configured transport";
  if (typeof window !== "undefined") {
    try { if (typeof connection.server === "string") serverName = new URL(connection.server, window.location.href).host; } catch { /* Fixed label for unsupported/custom URLs. */ }
  }
  return { ready, busy, status, snapshot, history, phrase, spending, runtime, progress, receipt, canCancelPayment, baseAction,
    clearReceipt: () => { setReceipt(undefined); },
    reviewPayment: (draft: SendDraft) => run("Reviewing", wallet => reviewSend(wallet, draft)),
    sendPayment: (review: SendReview, words: string) => run("Sending", async wallet => {
      const operation = new AbortController(); sendOperation.current = operation; setCanCancelPayment(true);
      try {
        setStatus("Checking payment…");
        await recheckReview(wallet, review);
        await wallet.unlock(words);
        operation.signal.throwIfAborted();
        setStatus("Proving payment · this can take a moment…");
        const sent = await wallet.send(review.to, review.amount, review.memo || undefined, {
          signal: operation.signal, beforeBroadcast: () => owner.current === wallet && !operation.signal.aborted &&
            Date.now() - review.reviewedAt < 5 * 60_000,
        });
        if (!sent.txid) throw new Error("Missing receipt");
        if (owner.current !== wallet) return;
        setReceipt({ txid: sent.txid, state: "pending" });
        setStatus("Payment submitted. Sync to check confirmation.");
        await refresh(wallet, sent);
      } catch (error) {
        if (owner.current === wallet && error instanceof WalletError && error.code === "broadcast_failed" && error.txid) {
          setReceipt({ txid: error.txid, state: "unknown" });
        }
        throw error;
      } finally {
        wallet.lock();
        if (sendOperation.current === operation) sendOperation.current = undefined;
        if (owner.current === wallet) setCanCancelPayment(false);
      }
    }),
    cancelPayment: () => { sendOperation.current?.abort(); setStatus("Stopping before submission · waiting for proof cleanup…"); },
    network: connection.network,
    server: serverName,
    hidePhrase: () => { if (snapshot) { pendingRecovery.acknowledge(snapshot.unifiedAddress); setStatus("Saving wallet…"); } },
    create: () => run("Creating", async wallet => {
      if (await wallet.load()) { setStatus("A wallet is already saved here."); return; }
      let committed = false;
      try {
        const created = await wallet.create({ birthday: "auto", beforeCommit: preparation => {
          if (owner.current !== wallet) throw new DOMException("Creation cancelled", "AbortError");
          setSnapshot(preparation.wallet);
          setStatus("Save your recovery phrase.");
          return pendingRecovery.prepare(preparation.wallet.unifiedAddress, preparation.recoveryPhrase, preparation.signal);
        } });
        committed = true;
        if (owner.current !== wallet) return;
        setStatus("Wallet created. Sync when ready.");
        await refresh(wallet, created.wallet);
      } catch (error) {
        if (!committed && owner.current === wallet) setSnapshot(undefined);
        throw error;
      }
    }),
    restore: (words: string, birthday: string) => run("Restoring", async wallet => {
      if (await wallet.load()) { setStatus("Remove the saved wallet before restoring another."); return; }
      const value = await wallet.restore(words, { birthday });
      if (owner.current !== wallet) return;
      pendingRecovery.acknowledge(value.unifiedAddress);
      setStatus("Wallet restored. Sync to recover activity.");
      await refresh(wallet, value);
    }),
    sync: () => run("Syncing", async wallet => {
      await refresh(wallet, await wallet.sync());
      if (receipt) {
        const updated = await refreshReceipt(wallet, receipt);
        if (owner.current === wallet) setReceipt(updated);
      }
      if (owner.current === wallet) {
        const tip = await wallet.tip();
        setStatus(tip.behind === 0 ? `Scanned through block ${tip.scanned.toLocaleString()}.` : `Scan stopped · ${tip.behind.toLocaleString()} blocks remaining.`);
      }
    }),
    rescan: (birthday: string) => run("Rescanning", async wallet => {
      wallet.lock();
      // Commit the new birthday first; cancellation of sync keeps a resumable wallet.
      await refresh(wallet, await wallet.rescan({ birthday }));
      await refresh(wallet, await wallet.sync());
      if (owner.current === wallet) {
        const tip = await wallet.tip();
        setStatus(tip.behind === 0 ? `Scanned through block ${tip.scanned.toLocaleString()}.` : `Scan stopped · ${tip.behind.toLocaleString()} blocks remaining.`);
      }
    }),
    cancel: () => { owner.current?.cancelSync(); },
    unlock: (words: string) => run("Unlocking", async wallet => {
      await refresh(wallet, await wallet.unlock(words));
      if (owner.current === wallet) setStatus("Spending unlocked. Lock when done.");
    }),
    lock: () => { owner.current?.lock(); setSpending(false); setStatus("Spending locked. Viewing data stays available."); },
    forget: () => run("Removing", async wallet => {
      await wallet.forget({ passkey: true });
      if (snapshot) pendingRecovery.acknowledge(snapshot.unifiedAddress);
      if (owner.current !== wallet) return;
      setSnapshot(undefined); setHistory([]); setReceipt(undefined);
      setStatus("Local wallet removed. You can create or restore.");
    }),
  };
}
