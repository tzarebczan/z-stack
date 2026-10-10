import { formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/sdk";
import type { WalletApp } from "./app-context";
import { element } from "./dom";
import { reviewSend, syncForReview, recheckReview, assertReviewCurrent, refreshReceipt, type SendReview, type SendReceipt } from "./send";
import { paymentErrorMessage } from "./wallet-view";

export function attachPayments(app: WalletApp) {
  const form = element("send-form", HTMLFormElement);
  const reviewPanel = element("send-review");
  const receiptPanel = element("send-receipt");
  const status = element("send-status");
  const words = element("send-words", HTMLTextAreaElement);
  const cancel = element("cancel-send", HTMLButtonElement);
  let review: SendReview | undefined;
  let receipt: SendReceipt | undefined;
  let receiptOwner: string | undefined;
  const previousReceipts: Array<{ owner: string; receipt: SendReceipt }> = [];
  let operation: AbortController | undefined;

  // The viewing key identifies an account even when its receive address changes.
  // It stays in memory and is never rendered or saved by this example.
  function account(snapshot: WalletSnapshot | null | undefined): string | undefined {
    return snapshot ? `${snapshot.network}:${snapshot.ufvk ?? snapshot.unifiedAddress}` : undefined;
  }
  function showPreviousReceipts() {
    element("previous-payments").hidden = previousReceipts.length === 0;
    const list = element("previous-payment-list");
    list.replaceChildren();
    for (const { receipt: previous } of previousReceipts) {
      const item = document.createElement("li");
      const id = document.createElement("code");
      id.textContent = previous.txid;
      item.append(id, ` · ${previous.state === "unknown" ? "Submission not confirmed" : "Awaiting confirmation"}`);
      list.append(item);
    }
  }

  function clearWords() {
    words.value = "";
    words.removeAttribute("aria-invalid");
  }
  function showReceipt(value: SendReceipt) {
    if (value.state === "mined") status.textContent = "Payment confirmed.";
    if (value.state === "expired") status.textContent = "Payment expired. Check your balance before another payment.";
    review = undefined;
    reviewPanel.hidden = true;
    form.hidden = true;
    receiptPanel.hidden = false;
    element("receipt-txid").textContent = value.txid;
    element("receipt-state").textContent = value.state === "mined" ? "Confirmed on-chain"
      : value.state === "expired" ? "Expired. Sync and check your balance before another payment."
      : value.state === "unknown" ? "Submission not confirmed. Sync and check this transaction before making another payment."
      : "Submitted · awaiting confirmation";
    element("another-send").hidden = value.state === "pending" || value.state === "unknown";
  }
  function showReview(value: SendReview) {
    const details = element("review-details");
    details.replaceChildren();
    for (const [label, text] of [
      ["To", value.to], ["Amount", `${value.amount} ${app.unit}`],
      ["Estimated fee", `${formatZatoshis(BigInt(value.feeZat))} ${app.unit}`], ["Memo", value.memo || "None"],
    ]) {
      const term = document.createElement("dt");
      const description = document.createElement("dd");
      term.textContent = label;
      description.textContent = text;
      details.append(term, description);
    }
    form.hidden = true;
    reviewPanel.hidden = false;
    status.textContent = "Check the recipient, amount and memo.";
    element("confirm-send").textContent = `Send ${value.amount} ${app.unit}`;
    reviewPanel.focus();
  }

  app.wallet.on("broadcast", () => {
    if (!operation) return;
    cancel.hidden = true;
    status.textContent = "Submitting payment…";
  });
  form.addEventListener("reset", () => {
    status.textContent = "";
    for (const field of form.querySelectorAll("[aria-invalid]")) field.removeAttribute("aria-invalid");
  });
  for (const target of [form, element("confirm-send-form")]) {
    target.addEventListener("input", event => {
      if (event.target instanceof HTMLElement) event.target.removeAttribute("aria-invalid");
      status.textContent = "";
      if (target === form) element("status").textContent = "";
    });
  }
  form.addEventListener("invalid", event => {
    if (event.target instanceof HTMLElement) event.target.setAttribute("aria-invalid", "true");
    status.textContent = "Complete the required payment fields.";
    element("status").textContent = "";
  }, true);
  form.addEventListener("submit", event => {
    event.preventDefault();
    if (!app.hasScanned) { status.textContent = "Sync the wallet before sending."; return; }
    const draft = {
      to: element("send-to", HTMLTextAreaElement).value,
      amount: element("send-amount", HTMLInputElement).value,
      memo: element("send-memo", HTMLTextAreaElement).value,
    };
    void app.run(async () => {
      review = await reviewSend(app.wallet, draft);
      showReview(review);
    }, status);
  });
  element("confirm-send-form", HTMLFormElement).addEventListener("submit", event => {
    event.preventDefault();
    const mnemonic = words.value.trim();
    words.value = "";
    const approved = review;
    const owner = account(app.snapshot);
    if (!approved || !owner || operation) return;
    void app.run(async () => {
      const sending = new AbortController();
      operation = sending;
      try {
        status.textContent = "Checking payment…";
        cancel.hidden = false;
        await app.render(await syncForReview(app.wallet, approved, sending.signal));
        await recheckReview(app.wallet, approved, sending.signal);
        sending.signal.throwIfAborted();
        await app.wallet.unlock(mnemonic);
        sending.signal.throwIfAborted();
        status.textContent = "Proving payment · this can take a moment…";
        cancel.hidden = false;
        const sent = await app.wallet.send(approved.to, approved.amount, approved.memo || undefined, {
          signal: sending.signal,
          beforeBroadcast: () => {
            if (review !== approved || sending.signal.aborted) return false;
            assertReviewCurrent(approved);
            return true;
          },
        });
        if (!sent.txid) throw new Error("Missing receipt");
        receipt = { txid: sent.txid, state: "pending" };
        receiptOwner = owner;
        showReceipt(receipt);
        status.textContent = "Payment submitted. Sync to check confirmation.";
        await app.render(sent);
      } catch (error) {
        if (error instanceof WalletError && error.code === "broadcast_failed" && error.txid) {
          receipt = { txid: error.txid, state: "unknown" };
          receiptOwner = owner;
          showReceipt(receipt);
        }
        const safe = WalletError.fromUnknown(error);
        if (safe.code === "wallet_changed") throw error;
        status.textContent = paymentErrorMessage(error, app.snapshot, app.unit);
        if (safe.code === "invalid_recovery_phrase" || safe.code === "seed_mismatch") words.setAttribute("aria-invalid", "true");
      } finally {
        app.wallet.lock();
        operation = undefined;
        cancel.hidden = true;
      }
    }, status);
  });
  element("max-send", HTMLButtonElement).addEventListener("click", () => void app.run(async () => {
    const to = element("send-to", HTMLTextAreaElement).value.trim();
    if (to.toLowerCase().startsWith("zcash:")) throw new WalletError("unsupported_payment_uri", "Paste the recipient address itself.");
    const maximum = await app.wallet.maxSend(to || undefined);
    const amount = formatZatoshis(BigInt(maximum.maxSendZat));
    element("send-amount", HTMLInputElement).value = amount;
    status.textContent = maximum.maxSendZat > 0
      ? `Available after the estimated fee: ${amount} ${app.unit}. Review before sending.`
      : "No funds are available after the estimated fee.";
  }, status));
  element("edit-send").addEventListener("click", () => {
    review = undefined;
    clearWords();
    status.textContent = "";
    reviewPanel.hidden = true;
    form.hidden = false;
    element("send-to").focus();
  });
  cancel.addEventListener("click", () => {
    operation?.abort();
    cancel.hidden = true;
    status.textContent = "Stopping before submission · waiting for proof cleanup…";
  });
  element("another-send").addEventListener("click", () => {
    receipt = undefined;
    receiptOwner = undefined;
    form.reset();
    receiptPanel.hidden = true;
    form.hidden = false;
    status.textContent = "";
  });
  return {
    get unresolved() { return receipt?.state === "pending" || receipt?.state === "unknown"; },
    clearWords,
    abort: () => operation?.abort(),
    async refresh() {
      if (!receipt || receiptOwner !== account(app.snapshot)) return;
      receipt = await refreshReceipt(app.wallet, receipt);
      showReceipt(receipt);
    },
    reset(options?: { savedWallet: WalletSnapshot | null }) {
      review = undefined;
      const nextOwner = account(options?.savedWallet);
      if (receipt && receiptOwner !== nextOwner) {
        if (receiptOwner && (receipt.state === "pending" || receipt.state === "unknown")) {
          previousReceipts.push({ owner: receiptOwner, receipt });
        }
        receipt = undefined;
        receiptOwner = undefined;
      }
      const previous = previousReceipts.findIndex(entry => entry.owner === nextOwner);
      if (!receipt && previous >= 0) {
        const restored = previousReceipts.splice(previous, 1)[0]!;
        receipt = restored.receipt;
        receiptOwner = restored.owner;
      }
      clearWords();
      form.reset();
      reviewPanel.hidden = true;
      receiptPanel.hidden = true;
      form.hidden = false;
      status.textContent = "";
      showPreviousReceipts();
      if (receipt) showReceipt(receipt);
    },
  };
}
