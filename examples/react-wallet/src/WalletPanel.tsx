import { useEffect, useRef, useState } from "react";
import { createWallet, formatZatoshis, walletErrorMessage, WalletError, type Wallet, type WalletSnapshot } from "@z-stack/sdk";
import { walletLifetime } from "./lifetime";

// No engine, storage or network work during import/SSR. One queue per app owner.
const acquire = walletLifetime(() => createWallet({ network: "testnet",
  server: "https://zcash-testnet.chainsafe.dev", autoSync: false, autoShield: false,
  memoFetch: "on-demand", unlockPolicy: "each-spend" }));

export function WalletPanel() {
  const owner = useRef<Wallet>();
  const running = useRef(false);
  const hasWallet = useRef(false);
  const confirmRecovery = useRef<(() => void) | undefined>();
  const cancelRecovery = useRef<(() => void) | undefined>();
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("Opening wallet…");
  const [snapshot, setSnapshot] = useState<WalletSnapshot>();
  const [ids, setIds] = useState<string[]>([]);
  const [phrase, setPhrase] = useState("");

  useEffect(() => {
    hasWallet.current = false;
    setReady(false); setBusy(false); setSnapshot(undefined); setIds([]); setPhrase(""); setStatus("Opening wallet…");
    const lease = acquire();
    let disposed = false;
    const unsubscribe: (() => void)[] = [];
    void lease.ready.then(async wallet => {
      if (disposed || !wallet) return;
      owner.current = wallet;
      unsubscribe.push(wallet.on("sync", progress => {
        // load() reports snapshot hydration through this event; it is not a sync.
        if (disposed || progress.heading === "Restoring snapshot") return;
        setStatus(progress.stage === "synced" ? "Up to date" : "Syncing…");
      }));
      const saved = await wallet.load();
      if (disposed) return;
      if (saved) {
        hasWallet.current = true;
        setSnapshot(saved);
        const entries = await wallet.history(20);
        if (disposed) return;
        setIds(entries.map(entry => entry.txid));
      }
      setStatus(saved ? "Wallet opened. Sync when ready." : "Create a testnet wallet.");
      setReady(true);
    }).catch(error => {
      if (!disposed) setStatus(error instanceof WalletError ? walletErrorMessage(error.code) : "Could not open wallet.");
    });
    const release = () => {
      if (disposed) return;
      disposed = true; owner.current = undefined;
      unsubscribe.forEach(off => off()); lease.release();
    };
    const onHide = () => { cancelRecovery.current?.(); owner.current?.lock(); setPhrase(""); };
    const onShow = (event: PageTransitionEvent) => {
      if (event.persisted && !disposed && owner.current) setStatus(hasWallet.current
        ? "Wallet opened. Spending is locked." : "Create a testnet wallet.");
    };
    window.addEventListener("pagehide", onHide);
    window.addEventListener("pageshow", onShow);
    return () => {
      window.removeEventListener("pagehide", onHide);
      window.removeEventListener("pageshow", onShow); release();
    };
  }, []);

  useEffect(() => {
    if (!phrase) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [phrase]);

  async function run(action: (wallet: Wallet) => Promise<WalletSnapshot>) {
    const wallet = owner.current;
    if (!wallet || running.current) return;
    running.current = true;
    setBusy(true);
    try {
      const next = await action(wallet);
      if (owner.current !== wallet) return;
      hasWallet.current = true;
      setSnapshot(next);
      const entries = await wallet.history(20);
      if (owner.current === wallet) setIds(entries.map(entry => entry.txid));
    } catch (error) {
      if (owner.current === wallet) setStatus(error instanceof WalletError ? walletErrorMessage(error.code) : "Could not complete action.");
    } finally {
      running.current = false;
      if (owner.current === wallet) setBusy(false);
    }
  }

  return <section aria-label="Testnet wallet">
    <p role="status">{status}</p>
    <p id="balance">{snapshot ? formatZatoshis(BigInt(snapshot.balance?.totalAvailable ?? 0)) : "—"} ZEC</p>
    <p id="address">{snapshot?.unifiedAddress}</p>
    <div className="actions">
      <button disabled={!ready || busy || !!snapshot} onClick={() => void run(async wallet => {
        const created = await wallet.create({ birthday: "auto", beforeCommit: preparation => {
          if (owner.current !== wallet) throw new DOMException("Creation cancelled", "AbortError");
          setPhrase(preparation.recoveryPhrase);
          setStatus("Save these words before creating your wallet.");
          return new Promise<void>((resolve, reject) => {
            const cleanup = () => {
              if (confirmRecovery.current === confirm) confirmRecovery.current = undefined;
              if (cancelRecovery.current === cancel) cancelRecovery.current = undefined;
              preparation.signal.removeEventListener("abort", cancel);
              if (owner.current === wallet) setPhrase("");
            };
            const cancel = () => { cleanup(); reject(new DOMException("Creation cancelled", "AbortError")); };
            const confirm = () => { cleanup(); setStatus("Saving wallet…"); resolve(); };
            confirmRecovery.current = confirm;
            cancelRecovery.current = cancel;
            preparation.signal.addEventListener("abort", cancel, { once: true });
            if (preparation.signal.aborted) cancel();
          });
        } });
        if (owner.current === wallet) setStatus("Wallet created. Sync when ready.");
        return created.wallet;
      })}>Create wallet</button>
      <button disabled={!ready || busy || !snapshot} onClick={() => void run(wallet => wallet.sync())}>Sync</button>
      <button disabled={!ready || busy || !snapshot} onClick={() => {
        owner.current?.lock();
        setPhrase("");
        setStatus("Spending locked. History stays visible.");
      }}>Lock</button>
    </div>
    {phrase && <><pre id="phrase">{phrase}</pre><button onClick={() => confirmRecovery.current?.()}>I saved these words — finish</button></>}
    <h2>Activity</h2>
    <ul>{ids.map(id => <li key={id}>{id}</li>)}</ul>
  </section>;
}
