import { classifyHistory, createWallet, formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/sdk";
import "./style.css";
import { attachBase } from "./base";
import { connection } from "./connection";
import { reviewSend, recheckReview, refreshReceipt, type SendReview, type SendReceipt } from "./send";

const element = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = element("status");
const balance = element("balance");
const address = element("address");
const history = element("history");
const phrase = element("phrase");
const hidePhrase = element<HTMLButtonElement>("hide-phrase");
const words = element<HTMLTextAreaElement>("words");
const birthday = element<HTMLInputElement>("birthday");
const create = element<HTMLButtonElement>("create");
const sync = element<HTMLButtonElement>("sync");
const lock = element<HTMLButtonElement>("lock");
const restore = element<HTMLButtonElement>("restore");
const clearWords = element<HTMLButtonElement>("clear-words");

async function start() {
  const wallet = await createWallet({
    ...connection,
    memoFetch: "on-demand",
    autoShield: false,
    unlockPolicy: "each-spend",
    // An example makes network work explicit; production apps may enable autoSync.
    autoSync: false,
  });

  let review: SendReview | undefined;
  let receipt: SendReceipt | undefined;
  let sending: AbortController | undefined;
  let busy = false;
  let identity: string | undefined;
  const sendWords = element<HTMLTextAreaElement>("send-words");
  const sendStatus = element("send-status");
  const sendForm = element<HTMLFormElement>("send-form");
  const reviewPanel = element("send-review");
  const receiptPanel = element("send-receipt");
  const cancelSend = element<HTMLButtonElement>("cancel-send");
  const offBroadcast = wallet.on("broadcast", () => {
    if (!sending) return;
    cancelSend.hidden = true;
    sendStatus.textContent = "Submitting payment…";
  });
  let creating = false;
  let confirmRecovery: (() => void) | undefined;

  const offSync = wallet.on("sync", progress => {
    status.textContent = progress.stage === "synced" ? "Up to date" : `Syncing · ${Math.round(progress.percent ?? 0)}%`;
  });
  const offBalance = wallet.on("balance", value => {
    balance.textContent = `${formatZatoshis(BigInt(value.availableZat))} ZEC`;
  });

  async function render(snapshot: WalletSnapshot) {
    identity = snapshot.unifiedAddress;
    element("send-panel").hidden = false;
    address.textContent = snapshot.unifiedAddress;
    if (receipt) {
      receipt = await refreshReceipt(wallet, receipt);
      showReceipt(receipt);
    }
    balance.textContent = `${formatZatoshis(BigInt(snapshot.balance?.totalAvailable ?? 0))} ZEC`;
    const entries = await wallet.history(20);
    const rows = entries.map(entry => {
      const row = document.createElement("li");
      row.textContent = `${classifyHistory(entry).action} · ${entry.txid}`;
      return row;
    });
    history.replaceChildren(...rows);
  }

  function clearPhrase() {
    phrase.textContent = "";
    phrase.hidden = true;
    hidePhrase.hidden = true;
    words.value = "";
    sendWords.value = "";
  }

  async function perform<T>(action: () => Promise<T>): Promise<T> {
    if (busy) throw new Error("Another wallet action is in progress.");
    busy = true;
    const controls = [...document.querySelectorAll<HTMLInputElement | HTMLButtonElement | HTMLTextAreaElement>("input, textarea, button")]
      .filter(control => ![hidePhrase, cancelSend].includes(control as HTMLButtonElement));
    for (const control of controls) control.disabled = true;
    try {
      return await action();
    } finally {
      for (const control of controls) control.disabled = false;
      busy = false;
    }
  }

  async function run(action: () => Promise<void>) {
    try { await perform(action); }
    catch (error) { status.textContent = error instanceof WalletError ? error.userMessage() : "Could not complete this action."; }
  }
  const disposeBase = attachBase(wallet, perform, () => identity);

  function showReceipt(value: SendReceipt) {
    review = undefined;
    reviewPanel.hidden = true;
    sendForm.hidden = true;
    receiptPanel.hidden = false;
    element("receipt-txid").textContent = value.txid;
    element("receipt-state").textContent = value.state === "mined" ? "Confirmed on-chain" : value.state === "expired"
      ? "Expired. Sync and check your balance before another payment." : value.state === "unknown"
      ? "Submission not confirmed. Sync and check this transaction before making another payment." : "Submitted · awaiting confirmation";
    element<HTMLButtonElement>("another-send").hidden = value.state === "unknown";
  }
  sendForm.addEventListener("submit", event => {
    event.preventDefault();
    const draft = { to: element<HTMLTextAreaElement>("send-to").value,
      amount: element<HTMLInputElement>("send-amount").value, memo: element<HTMLTextAreaElement>("send-memo").value };
    void run(async () => {
      review = await reviewSend(wallet, draft);
      const details = element("review-details");
      details.replaceChildren();
      for (const [label, value] of [["To", review.to], ["Amount", `${review.amount} ZEC`],
        ["Estimated fee", `${formatZatoshis(BigInt(review.feeZat))} ZEC`], ["Memo", review.memo || "None"]]) {
        const term = document.createElement("dt"), description = document.createElement("dd");
        term.textContent = label; description.textContent = value; details.append(term, description);
      }
      sendForm.hidden = true; reviewPanel.hidden = false;
      sendStatus.textContent = "Check the recipient, amount and memo.";
      element<HTMLButtonElement>("confirm-send").textContent = `Send ${review.amount} ZEC`;
      reviewPanel.focus();
    });
  });
  element<HTMLFormElement>("confirm-send-form").addEventListener("submit", event => {
    event.preventDefault();
    const mnemonic = sendWords.value.trim(); sendWords.value = "";
    const approved = review;
    if (!approved || sending) return;
    void run(async () => {
      const operation = new AbortController(); sending = operation;
      try {
        sendStatus.textContent = "Checking payment…";
        await recheckReview(wallet, approved);
        await wallet.unlock(mnemonic);
        if (operation.signal.aborted) throw new DOMException("Cancelled", "AbortError");
        sendStatus.textContent = "Proving payment · this can take a moment…";
        cancelSend.hidden = false;
        const sent = await wallet.send(approved.to, approved.amount, approved.memo || undefined, {
          signal: operation.signal, beforeBroadcast: () => review === approved && !operation.signal.aborted && Date.now() - approved.reviewedAt < 5 * 60_000,
        });
        if (!sent.txid) throw new Error("Missing receipt");
        receipt = { txid: sent.txid, state: "pending" }; showReceipt(receipt);
        sendStatus.textContent = "Payment submitted. Sync to check confirmation.";
        await render(sent);
      } catch (error) {
        if (error instanceof WalletError && error.code === "broadcast_failed" && error.txid) {
          receipt = { txid: error.txid, state: "unknown" }; showReceipt(receipt);
        }
        sendStatus.textContent = WalletError.fromUnknown(error).userMessage();
      } finally { wallet.lock(); sending = undefined; cancelSend.hidden = true; }
    });
  });
  element("edit-send").addEventListener("click", () => {
    review = undefined; sendWords.value = ""; reviewPanel.hidden = true; sendForm.hidden = false;
    element("send-to").focus();
  });
  cancelSend.addEventListener("click", () => {
    sending?.abort(); cancelSend.hidden = true; sendStatus.textContent = "Stopping before submission · waiting for proof cleanup…";
  });
  element("another-send").addEventListener("click", () => {
    receipt = undefined; sendForm.reset(); receiptPanel.hidden = true; sendForm.hidden = false; sendStatus.textContent = "";
  });

  create.addEventListener("click", () => void run(async () => {
    if (await wallet.load()) {
      status.textContent = "A wallet is already saved here. Back it up before replacing it.";
      return;
    }
    creating = true;
    try {
      const created = await wallet.create({ birthday: "auto", beforeCommit: preparation => {
        // No wallet snapshot exists yet. Leaving now cancels creation rather
        // than storing a wallet whose recovery phrase has never been saved.
        phrase.textContent = preparation.recoveryPhrase;
        phrase.hidden = false;
        hidePhrase.hidden = false;
        status.textContent = "Save these words, then finish creating your wallet.";
        return new Promise<void>((resolve, reject) => {
          const cleanup = () => { confirmRecovery = undefined; preparation.signal.removeEventListener("abort", cancel); clearPhrase(); };
          const cancel = () => { cleanup(); reject(new DOMException("Creation cancelled", "AbortError")); };
          confirmRecovery = () => { cleanup(); status.textContent = "Saving wallet…"; resolve(); };
          preparation.signal.addEventListener("abort", cancel, { once: true });
          if (preparation.signal.aborted) cancel();
        });
      } });
      await render(created.wallet);
      status.textContent = "Wallet created. Sync when ready.";
    } finally { creating = false; clearPhrase(); }
  }));

  sync.addEventListener("click", () => void run(async () => {
    // Retain the previous balance and history while syncing.
    const snapshot = await wallet.sync();
    await render(snapshot);
  }));

  element<HTMLFormElement>("restore-form").addEventListener("submit", event => {
    event.preventDefault();
    const mnemonic = words.value.trim();
    const restoreBirthday = birthday.value.trim();
    words.value = "";
    void run(async () => {
      // This sample does not implement a replacement-confirmation flow.
      if (await wallet.load()) {
        status.textContent = "A wallet is already saved here. Use another browser profile to try recovery.";
        return;
      }
      const restored = await wallet.restore(mnemonic, { birthday: restoreBirthday });
      clearPhrase();
      await render(restored);
      status.textContent = "Wallet restored. Sync to recover activity.";
    });
  });
  lock.addEventListener("click", () => {
    wallet.lock(); clearPhrase(); status.textContent = "Spending locked. History stays visible.";
  });
  hidePhrase.addEventListener("click", () => confirmRecovery?.());
  clearWords.addEventListener("click", clearPhrase);
  window.addEventListener("beforeunload", event => {
    if (creating) { event.preventDefault(); event.returnValue = ""; }
  });

  window.addEventListener("pagehide", () => {
    identity = undefined; disposeBase();
    clearPhrase();
    words.value = "";
    offSync();
    offBalance();
    offBroadcast();
    sending?.abort();
    void wallet.close().catch(() => {});
  }, { once: true });

  await run(async () => {
    const saved = await wallet.load();
    if (saved) await render(saved);
    status.textContent = saved ? "Wallet opened. Sync when ready." : "Create a wallet or restore one.";
  });
}

void start().catch(() => {
  status.textContent = "Could not load the wallet. Check your browser settings and connection, then reload.";
  for (const button of [create, sync, restore, lock]) button.disabled = true;
});
