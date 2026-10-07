"use client";
import { useState } from "react";
import { classifyHistory, formatZatoshis } from "@z-stack/sdk";
import { useWallet } from "../lib/use-wallet";
import { SecretForm } from "./SecretForm";
import { SetupChecks } from "./SetupChecks";
import { BaseWallet } from "./BaseWallet";
import { SendPayment } from "./SendPayment";

export function WalletScreen() {
  const wallet = useWallet();
  const [saved, setSaved] = useState(false);
  const [remove, setRemove] = useState(false);
  const [copied, setCopied] = useState("");
  const disabled = !wallet.ready || !!wallet.busy;
  const backup = !!wallet.phrase;
  return <>
    <div className="title-row"><div><p className="eyebrow">Your local wallet</p><h1>A little pocket of privacy.</h1></div>
      <span className="network">{wallet.network === "testnet" ? "Testnet" : "Regtest fixture"}</span></div>
    <div className="workspace">
      <section className="wallet-main" aria-label="Wallet">
        <div className="balance-block"><p className="eyebrow">Available balance</p>
          <p className="balance" id="balance">{wallet.snapshot ? formatZatoshis(BigInt(wallet.snapshot.balance?.totalAvailable ?? 0)) : "—"}
            <span>ZEC</span></p>
          <div className="wallet-state"><span>{wallet.spending ? "Spending unlocked" : "Spending locked"}</span>
            <span>{wallet.runtime}</span></div>
        </div>
        <p role="status" className="status">{wallet.status}</p>
        {!wallet.snapshot ? <div className="welcome">
          <h2>Start with test funds.</h2><p>Create a wallet or bring your saved phrase. No sign-up needed.</p>
          <button className="primary" disabled={disabled} onClick={() => {
            setSaved(false); void wallet.create();
          }}>{wallet.busy === "Creating" ? "Creating…" : "Create wallet"}</button>
          <details><summary>Restore an existing wallet</summary>
            <SecretForm restore disabled={disabled} submit={wallet.restore} /></details>
        </div> : <>
          <div className="actions sync-actions"><button className="primary" disabled={disabled || backup}
            onClick={() => void wallet.sync()}>Sync wallet</button>
            {wallet.busy === "Syncing" && <button onClick={wallet.cancel}>Stop sync</button>}
            <button disabled={disabled || backup || !wallet.spending} onClick={wallet.lock}>Lock spending</button>
            {wallet.busy && <span className="sync-indicator" role="status">{wallet.busy}
              {wallet.busy === "Syncing" && wallet.progress !== undefined ? ` · ${Math.round(wallet.progress)}%` : "…"}</span>}
          </div>
          {backup && <section className="backup" aria-label="Recovery backup"><h2>Save these 24 words.</h2>
            <p>This is your recovery phrase. Keep it somewhere private.</p>
            <ol className="words">{wallet.phrase.split(" ").map((word, index) => <li key={index}>{word}</li>)}</ol>
            <label className="checkbox"><input type="checkbox" checked={saved} onChange={event => setSaved(event.target.checked)} />
              I saved my recovery phrase</label>
            <button disabled={!saved} onClick={wallet.hidePhrase}>Done, hide phrase</button></section>}
          {!backup && !wallet.spending && <details><summary>Unlock with your recovery phrase</summary>
            <SecretForm disabled={disabled} submit={words => wallet.unlock(words)} /></details>}
          {!backup && <SendPayment key={wallet.snapshot.unifiedAddress} disabled={disabled} canCancel={wallet.canCancelPayment}
            receipt={wallet.receipt} reviewPayment={wallet.reviewPayment} sendPayment={wallet.sendPayment}
            cancelPayment={wallet.cancelPayment} clearReceipt={wallet.clearReceipt} />}
          {!backup && <BaseWallet identity={wallet.snapshot.unifiedAddress} disabled={disabled} withWallet={wallet.baseAction} />}
          <section className="activity" aria-label="Activity"><div className="section-title"><h2>Activity</h2>
            <span className="hint">{wallet.snapshot.scannedHeight ? `Scanned to ${wallet.snapshot.scannedHeight.toLocaleString()}` : "Sync to update"}</span></div>
            {wallet.history.length ? <ul>{wallet.history.map(entry => <li key={entry.txid}>
              <strong>{classifyHistory(entry).action}</strong><code>{entry.txid}</code></li>)}</ul>
              : <div className="empty"><span className="empty-mark" aria-hidden="true">↗</span><p>No activity yet.</p>
                <p className="hint">Receive test ZEC, then sync to see it here.</p></div>}
          </section>
        </>}
      </section>
      <aside>
        <section className="receive"><p className="eyebrow">Receive</p><h2>Your private address</h2>
          {wallet.snapshot ? <><p className="hint">Use this unified address for shielded test deposits.</p>
            <code id="address">{wallet.snapshot.unifiedAddress}</code>
            <button onClick={() => {
              void (async () => {
                try { await navigator.clipboard.writeText(wallet.snapshot!.unifiedAddress); setCopied("Address copied."); }
                catch { setCopied("Could not copy. Select the address instead."); }
              })();
            }}>Copy address</button><p role="status" className="hint">{copied}</p></>
            : <p className="hint">Your address will appear after you create or restore a wallet.</p>}
        </section>
        <SetupChecks />
        {wallet.snapshot && !backup && <details className="remove"><summary>Remove local wallet</summary>
          <p className="hint">Deletes this browser’s saved copy. Your funds stay on-chain.</p>
          <label className="checkbox"><input type="checkbox" checked={remove} onChange={event => setRemove(event.target.checked)} />
            I have the recovery phrase</label>
          <button className="danger" disabled={disabled || !remove} onClick={() => {
            void wallet.forget().then(() => { setRemove(false); setCopied(""); });
          }}>Remove from this browser</button></details>}
      </aside>
    </div>
  </>;
}
