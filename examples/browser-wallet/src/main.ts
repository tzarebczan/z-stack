import { classifyHistory, createWallet, validateBirthdayInput, formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/sdk";
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
const copyAddress = element<HTMLButtonElement>("copy-address");
const copyPhrase = element<HTMLButtonElement>("copy-phrase");
const restoreForm = element<HTMLFormElement>("restore-form");

async function start() {
  const wallet = await createWallet({
    ...connection,
    memoFetch: "on-demand",
    autoShield: false,
    unlockPolicy: "each-spend",
    // An example makes network work explicit; production apps may enable autoSync.
    autoSync: false,
    threads: 2,
  });

  const unit = connection.network === "testnet" ? "TAZ" : "ZEC";
  balance.textContent = `— ${unit}`;
  element("send-panel").setAttribute("aria-label", `Send ${unit}`);
  for (const el of document.querySelectorAll("h2, label")) if (el.textContent?.includes("ZEC")) el.textContent = el.textContent.replace("ZEC", unit);
  let server = "Configured transport";
  try { if (typeof connection.server === "string") server = new URL(connection.server, window.location.href).host; } catch { /* Keep a fixed label; never render credential-bearing URLs. */ }
  let review: SendReview | undefined;
  let receipt: SendReceipt | undefined;
  let sending: AbortController | undefined;
  let busy = false;
  let identity: string | undefined;
  let hasScanned = false;
  let recoveryPhrase = "";
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
    balance.textContent = `${formatZatoshis(BigInt(value.availableZat))} ${unit}`;
  });

  async function render(snapshot: WalletSnapshot) {
    identity = snapshot.unifiedAddress;
    hasScanned = (snapshot.scannedHeight ?? 0) >= snapshot.birthdayHeight;
    copyAddress.hidden = false;
    element("scan-panel").hidden = false;
    element("scan-details").textContent = `${connection.network} · ${server} · Birthday ${snapshot.birthdayHeight.toLocaleString()} · Scanned through ${(snapshot.scannedHeight ?? 0).toLocaleString()}`;
    element("send-panel").hidden = false;
    address.textContent = snapshot.unifiedAddress;
    if (receipt) {
      receipt = await refreshReceipt(wallet, receipt);
      showReceipt(receipt);
    }
    balance.textContent = `${formatZatoshis(BigInt(snapshot.balance?.totalAvailable ?? 0))} ${unit}`;
    const entries = await wallet.history(20);
    const rows = entries.map(entry => {
      const row = document.createElement("li");
      row.textContent = `${classifyHistory(entry).action} · ${entry.txid}`;
      return row;
    });
    history.replaceChildren(...rows);
    element("history-empty").hidden = rows.length > 0;
    element("history-empty").textContent = hasScanned ? "No activity yet." : "Sync to look for activity.";
    updateControls();
  }

  function clearPhrase() {
    phrase.replaceChildren();
    recoveryPhrase = "";
    copyPhrase.hidden = true;
    element("phrase-copy-status").textContent = "";
    element("phrase-copy-status").hidden = true;
    restoreForm.hidden = false;
    phrase.hidden = true;
    hidePhrase.hidden = true;
    words.value = "";
    sendWords.value = "";
  }

  function updateControls() {
    create.disabled = busy || !!identity;
    sync.disabled = busy || !identity;
    lock.disabled = busy || !identity;
    element<HTMLButtonElement>("review-send").disabled = busy || !identity || !hasScanned;
    element("send-readiness").hidden = hasScanned;
    copyAddress.disabled = busy || !identity;
  }

  async function copy(text: string, output: HTMLElement, secret = false) {
    // Clipboard writes are explicit user actions; no secret is copied automatically.
    try {
      await navigator.clipboard.writeText(text);
      output.textContent = secret ? "Phrase copied. Your clipboard now contains your recovery words." : "Address copied.";
    } catch { output.textContent = "Could not copy. Select the text instead."; }
  }
  copyAddress.addEventListener("click", () => { if (identity) void copy(identity, element("copy-status")); });
  copyPhrase.addEventListener("click", () => { if (recoveryPhrase) void copy(recoveryPhrase, element("phrase-copy-status"), true); });

  async function perform<T>(action: () => Promise<T>): Promise<T> {
    if (busy) throw new Error("Another wallet action is in progress.");
    busy = true;
    const controls = [...document.querySelectorAll<HTMLInputElement | HTMLButtonElement | HTMLTextAreaElement>("input, textarea, button")]
      .filter(control => ![hidePhrase, cancelSend, copyPhrase].includes(control as HTMLButtonElement));
    for (const control of controls) control.disabled = true;
    try {
      return await action();
    } finally {
      for (const control of controls) control.disabled = false;
      busy = false;
      updateControls();
    }
  }

  async function run(action: () => Promise<void>) {
    try { await perform(action); }
    catch (error) {
      const safe = WalletError.fromUnknown(error);
      console.warn("Wallet action failed", { code: safe.code });
      status.textContent = safe.userMessage();
    }
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
  sendForm.addEventListener("input", () => { sendStatus.textContent = ""; status.textContent = ""; });
  sendForm.addEventListener("invalid", () => { sendStatus.textContent = "Complete the required payment fields."; status.textContent = ""; }, true);
  sendForm.addEventListener("submit", event => {
    event.preventDefault();
    if (!hasScanned) { sendStatus.textContent = "Sync the wallet before sending."; return; }
    const draft = { to: element<HTMLTextAreaElement>("send-to").value,
      amount: element<HTMLInputElement>("send-amount").value, memo: element<HTMLTextAreaElement>("send-memo").value };
    void run(async () => {
      review = await reviewSend(wallet, draft);
      const details = element("review-details");
      details.replaceChildren();
      for (const [label, value] of [["To", review.to], ["Amount", `${review.amount} ${unit}`],
        ["Estimated fee", `${formatZatoshis(BigInt(review.feeZat))} ${unit}`], ["Memo", review.memo || "None"]]) {
        const term = document.createElement("dt"), description = document.createElement("dd");
        term.textContent = label; description.textContent = value; details.append(term, description);
      }
      sendForm.hidden = true; reviewPanel.hidden = false;
      sendStatus.textContent = "Check the recipient, amount and memo.";
      element<HTMLButtonElement>("confirm-send").textContent = `Send ${review.amount} ${unit}`;
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
        recoveryPhrase = preparation.recoveryPhrase;
        phrase.replaceChildren(...recoveryPhrase.split(/\s+/).map(word => {
          const item = document.createElement("li"); item.textContent = word + " "; return item;
        }));
        restoreForm.hidden = true;
        copyPhrase.hidden = false;
        element("phrase-copy-status").hidden = false;
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

  restoreForm.addEventListener("submit", event => {
    event.preventDefault();
    const mnemonic = words.value.trim();
    const restoreBirthday = birthday.value.trim();
    try { validateBirthdayInput(restoreBirthday); }
    catch (error) { birthday.setCustomValidity(WalletError.fromUnknown(error).userMessage()); birthday.reportValidity(); return; }
    void run(async () => {
      // This sample does not implement a replacement-confirmation flow.
      if (await wallet.load()) {
        status.textContent = "A wallet is already saved here. Use another browser profile to try recovery.";
        return;
      }
      words.value = ""; // Only clear after the saved-wallet guard accepts this attempt.
      const restored = await wallet.restore(mnemonic, { birthday: restoreBirthday });
      clearPhrase();
      await render(restored);
      status.textContent = "Wallet restored. Sync to recover activity.";
    });
  });
  birthday.addEventListener("input", () => birthday.setCustomValidity(""));
  const rescanBirthday = element<HTMLInputElement>("rescan-birthday");
  rescanBirthday.addEventListener("input", () => rescanBirthday.setCustomValidity(""));
  element<HTMLFormElement>("rescan-form").addEventListener("submit", event => {
    event.preventDefault();
    try { validateBirthdayInput(rescanBirthday.value); }
    catch (error) { rescanBirthday.setCustomValidity(WalletError.fromUnknown(error).userMessage()); rescanBirthday.reportValidity(); return; }
    element<HTMLInputElement>("rescan-confirm").checked = false;
    void run(async () => {
      wallet.lock();
      await render(await wallet.rescan({ birthday: rescanBirthday.value.trim() }));
      await render(await wallet.sync());
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

void start().catch(error => {
  const safe = WalletError.fromUnknown(error);
  // Never log raw errors: providers may include addresses, keys or request bodies.
  console.error("Wallet startup failed", { code: safe.code });
  status.textContent = `${safe.userMessage()} Reload to try again.`;
  for (const button of [create, sync, restore, lock]) button.disabled = true;
});
