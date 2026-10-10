"use client";
import { useEffect, useRef, useState } from "react";
import { formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/sdk";
import { paymentErrorMessage } from "../lib/wallet-view";
import type { SendDraft, SendReview, SendReceipt } from "../lib/send";

export function SendPayment({ snapshot, unit, pendingAmount, disabled, canReview, clearStatus, canCancel, receipt, reviewPayment, sendPayment, cancelPayment, clearReceipt }: {
  snapshot: WalletSnapshot; unit: string; pendingAmount?: string; disabled: boolean; canReview: boolean; clearStatus(): void; canCancel: boolean; receipt?: SendReceipt;
  reviewPayment(draft: SendDraft): Promise<SendReview | undefined>;
  sendPayment(review: SendReview, words: string): Promise<void | undefined>;
  cancelPayment(): void; clearReceipt(): void;
}) {
  const [status, setStatus] = useState("");
  const [invalidField, setInvalidField] = useState<string>();
  const clearError = () => { setStatus(""); setInvalidField(undefined); clearStatus(); };
  const [review, setReview] = useState<SendReview>();
  const [draft, setDraft] = useState<SendDraft>({ to: "", amount: "", memo: "" });
  const words = useRef<HTMLTextAreaElement>(null);
  const panel = useRef<HTMLElement>(null);
  const clearWords = () => { if (words.current) words.current.value = ""; };
  function showError(error: unknown) {
    clearStatus();
    setStatus(paymentErrorMessage(error, snapshot, unit));
    const code = WalletError.fromUnknown(error).code;
    setInvalidField(code === "invalid_address" ? "send-to" : code === "invalid_amount" ? "send-amount" : code === "invalid_memo" ? "send-memo" : undefined);
    if (code === "invalid_recovery_phrase" || code === "seed_mismatch") words.current?.setAttribute("aria-invalid", "true");
  }
  useEffect(() => {
    window.addEventListener("pagehide", clearWords);
    return () => { clearWords(); window.removeEventListener("pagehide", clearWords); };
  }, []);
  useEffect(() => { if (review) panel.current?.focus(); }, [review]);
  if (receipt) return <section className="payment receipt" aria-label="Payment receipt">
    <h2>Payment receipt</h2>
    <p data-testid="receipt-state" role="status">{receipt.state === "mined" ? "Confirmed on-chain" : receipt.state === "expired" ? "Expired · sync and check your balance."
      : receipt.state === "unknown" ? "Submission not confirmed. Sync and check this transaction before making another payment."
      : "Submitted · awaiting confirmation"}</p>
    <code data-testid="receipt-txid">{receipt.txid}</code>
    <p id="send-status" role="status">{status}</p>
    {receipt.state !== "unknown" && <button disabled={disabled} onClick={() => {
      setReview(undefined); setDraft({ to: "", amount: "", memo: "" }); clearReceipt();
    }}>New payment</button>}
  </section>;
  return <section className="payment" aria-label={`Send ${unit}`} ref={panel} tabIndex={-1}>
    {pendingAmount && <p id="send-confirming" className="hint">{pendingAmount} {unit} is still confirming. Only available funds can be spent.</p>}
    <h2>{review ? "Review payment" : `Send ${unit}`}</h2>
    {review ? <>
      <dl><dt>To</dt><dd>{review.to}</dd><dt>Amount</dt><dd>{review.amount} {unit}</dd>
        <dt>Estimated fee</dt><dd>{formatZatoshis(BigInt(review.feeZat))} {unit}</dd>
        <dt>Memo</dt><dd>{review.memo || "None"}</dd></dl>
      <p className="hint">Fee is estimated. The wallet checks it again before proving.</p>
      <form onInput={() => { clearError(); words.current?.removeAttribute("aria-invalid"); }} onSubmit={event => {
        event.preventDefault(); const phrase = words.current?.value.trim() ?? ""; clearWords();
        void sendPayment(review, phrase).catch(showError);
      }}>
        <label htmlFor="send-words">Recovery phrase for this payment</label>
        <textarea id="send-words" ref={words} aria-describedby="send-status" required disabled={disabled} rows={3}
          autoComplete="off" autoCapitalize="none" spellCheck={false} autoCorrect="off" />
        <p className="hint">Used locally and cleared when you send.</p>
        <div className="actions"><button className="primary" disabled={disabled}>Send {review.amount} {unit}</button>
          <button type="button" disabled={disabled} onClick={() => { clearWords(); setReview(undefined); }}>Edit</button>
          {canCancel && <button type="button" onClick={cancelPayment}>Cancel before submission</button>}</div>
      </form>
    </> : <form onInput={clearError} onInvalidCapture={event => { clearStatus(); setStatus("Complete the required payment fields."); if (event.target instanceof HTMLElement) setInvalidField(event.target.id); }} onSubmit={event => {
      event.preventDefault(); if (!canReview) return; void reviewPayment(draft).then(value => { if (value) { setStatus(""); setReview(value); } }).catch(showError);
    }}>
      <label htmlFor="send-to">Recipient address</label>
      <textarea id="send-to" name="recipient" aria-describedby="send-status" aria-invalid={invalidField === "send-to" || undefined} required disabled={disabled} rows={2} autoComplete="off"
        autoCapitalize="none" spellCheck={false} value={draft.to} onChange={event => setDraft({ ...draft, to: event.target.value })} />
      <label htmlFor="send-amount">Amount ({unit})</label>
      <input id="send-amount" name="amount" aria-describedby="send-status" aria-invalid={invalidField === "send-amount" || undefined} required disabled={disabled} inputMode="decimal" autoComplete="off"
        value={draft.amount} onChange={event => setDraft({ ...draft, amount: event.target.value })} />
      <label htmlFor="send-memo">Memo (optional)</label>
      <textarea id="send-memo" name="memo" aria-describedby="send-status" aria-invalid={invalidField === "send-memo" || undefined} disabled={disabled} rows={2} maxLength={512}
        value={draft.memo} onChange={event => setDraft({ ...draft, memo: event.target.value })} />
      <p className="hint" hidden={canReview}>Sync the wallet before reviewing a payment.</p>
      <button className="primary" disabled={disabled || !canReview}>Review payment</button>
    </form>}
    <p id="send-status" role="status">{status}</p>
  </section>;
}
