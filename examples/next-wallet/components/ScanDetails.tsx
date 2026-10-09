"use client";
import { useRef, useState } from "react";
import { validateBirthdayInput, WalletError, type WalletSnapshot } from "@z-stack/sdk";

export function ScanDetails({ snapshot, server, disabled, rescan }: {
  snapshot: WalletSnapshot; server: string; disabled: boolean;
  rescan(birthday: string): Promise<void | undefined>;
}) {
  const birthday = useRef<HTMLInputElement>(null);
  const [confirmed, setConfirmed] = useState(false);
  return <section className="scan-details" aria-label="Scan details">
    <dl><dt>Light server</dt><dd>{server}</dd><dt>Wallet birthday</dt>
      <dd>{snapshot.birthdayHeight.toLocaleString()}</dd><dt>Scanned through</dt>
      <dd>{(snapshot.scannedHeight ?? 0) >= snapshot.birthdayHeight ? snapshot.scannedHeight!.toLocaleString() : "Not scanned yet"}</dd></dl>
    <p className="hint">Missing a deposit? Check its confirmation on this server’s chain.
      Deposits below the birthday need an earlier scan. A rescan cannot find a payment on another chain.</p>
    <details><summary>Scan an earlier range</summary>
      <form onSubmit={event => {
        event.preventDefault(); const input = birthday.current!;
        try { validateBirthdayInput(input.value); }
        catch (error) { input.setCustomValidity(WalletError.fromUnknown(error).userMessage()); input.reportValidity(); return; }
        setConfirmed(false); void rescan(input.value.trim());
      }}>
        <label htmlFor="rescan-birthday">Earlier height or date</label>
        <input id="rescan-birthday" ref={birthday} required disabled={disabled} placeholder="Height or YYYY-MM-DD"
          aria-describedby="rescan-hint" onInput={event => event.currentTarget.setCustomValidity("")} />
        <p className="hint" id="rescan-hint">Keeps your wallet and address. Rebuilds activity and balance from this range.
          Pending outgoing payments must confirm or expire first. Large ranges need deepSync enabled by the app.</p>
        <label className="checkbox"><input type="checkbox" checked={confirmed} disabled={disabled}
          onChange={event => setConfirmed(event.target.checked)} />I want to rebuild scan history</label>
        <button disabled={disabled || !confirmed}>Rescan wallet</button>
      </form>
    </details>
  </section>;
}
