import { SDK_VERSION, classifyHistory, createWallet, validateBirthdayInput, formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/sdk";
import "./style.css";
import buildInfo from "../sdk-build.json";
import { pendingFunds, confirmationLabel, loadedWalletStatus, activityMovement, paymentErrorMessage, syncedWalletStatus, confirmationPolicyText, memoDetailsMessage } from "./wallet-view";
import { drawReceiveQr } from "./receive-qr";
import { attachBase } from "./base";
import { connection } from "./connection";
import { engineLoading } from "./engine-loading";
import { reviewSend, recheckReview, refreshReceipt, type SendReview, type SendReceipt } from "./send";

const sdkBuild: { version: string; revision: string | null } = buildInfo;

const element = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = element("status");
element("sdk-build").textContent = `SDK ${SDK_VERSION} · ${sdkBuild.version === SDK_VERSION && sdkBuild.revision ? "source " + sdkBuild.revision.slice(0, 12) : "local archives"}`;
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
const createBirthday = element<HTMLInputElement>("create-birthday");
const remove = element<HTMLButtonElement>("remove");
const removeConfirm = element<HTMLInputElement>("remove-confirm");
const restoreForm = element<HTMLFormElement>("restore-form");

async function start() {
  const onLoadProgress = engineLoading(element("engine-progress"), element<HTMLProgressElement>("engine-progress-bar"));
  const wallet = await createWallet({
    ...connection,
    memoFetch: "on-demand",
    autoShield: false,
    unlockPolicy: "each-spend",
    // An example makes network work explicit; production apps may enable autoSync.
    autoSync: false,
    threads: 2,
    onLoadProgress,
  });

  const unit = connection.network === "testnet" ? "TAZ" : "ZEC";
  element("title").textContent = `${connection.network[0].toUpperCase()}${connection.network.slice(1)} wallet`;
  balance.textContent = `— ${unit}`;
  element("send-panel").setAttribute("aria-label", `Send ${unit}`);
  for (const el of document.querySelectorAll("h2, label")) if (/ZEC|TAZ/.test(el.textContent ?? "")) el.textContent = el.textContent!.replace(/ZEC|TAZ/g, unit);
  let server = "Configured transport";
  try { if (typeof connection.server === "string") server = new URL(connection.server, window.location.href).host; } catch { /* Keep a fixed label; never render credential-bearing URLs. */ }
  element("chain-warning").hidden = connection.network !== "testnet";
  function showRuntime() {
    const runtime = wallet.runtime;
    element("runtime").textContent = runtime.scanner === "starting" ? "Starting scanner…" : runtime.mode === "multi-thread"
      ? `Threaded scanner · ${runtime.threads} threads` : "Single-thread engine";
  }
  showRuntime();
  wallet.on("runtime", () => showRuntime());
  let review: SendReview | undefined;
  let receipt: SendReceipt | undefined;
  let sending: AbortController | undefined;
  let busy = false;
  let identity: string | undefined;
  let hasScanned = false;
  let latestSnapshot: WalletSnapshot | undefined;
  let confirmationPolicy: WalletSnapshot["confirmations"];
  let pendingPayment = false;
  let recoveryPhrase = "";
  const sendWords = element<HTMLTextAreaElement>("send-words");
  const sendStatus = element("send-status");
  const sendForm = element<HTMLFormElement>("send-form");
  const reviewPanel = element("send-review");
  const receiptPanel = element("send-receipt");
  const cancelSend = element<HTMLButtonElement>("cancel-send");
  wallet.on("broadcast", () => {
    if (!sending) return;
    cancelSend.hidden = true;
    sendStatus.textContent = "Submitting payment…";
  });
  let creating = false;
  let restoreValidation = "";
  function clearRestoreValidation() {
    if (restoreValidation && status.textContent === restoreValidation) status.textContent = "";
    restoreValidation = "";
  }
  function showRestoreValidation(message: string) {
    restoreValidation = message;
    status.textContent = message;
  }
  let confirmRecovery: (() => void) | undefined;
  let cancelRecovery: (() => void) | undefined;

  wallet.on("sync", progress => {
    status.textContent = progress.stage === "synced" ? `Synced through block ${progress.scanned?.toLocaleString() ?? "unknown"}. Sync again for payments mined later.` : `Syncing · ${Math.round(progress.percent ?? 0)}%`;
  });
  function showBalance(available: bigint, pending: bigint, policy?: WalletSnapshot["confirmations"]) {
    balance.textContent = `${formatZatoshis(available)} ${unit}`;
    element("pending-balance").hidden = pending === 0n;
    const pendingText = `Confirming · ${formatZatoshis(pending)} ${unit}`;
    if (element("pending-balance").textContent !== pendingText) element("pending-balance").textContent = pendingText;
    element("pending-help").hidden = pending === 0n;
    element("pending-help").textContent = `Confirming funds cannot be spent yet. ${confirmationPolicyText(policy)}`;
    element("send-confirming").hidden = pending === 0n;
    element("send-confirming").textContent = `${formatZatoshis(pending)} ${unit} is still confirming. Only available funds can be spent.`;
  }
  wallet.on("balance", value => showBalance(BigInt(value.availableZat), BigInt(value.pendingZat ?? 0), confirmationPolicy));


  async function render(snapshot: WalletSnapshot) {
    latestSnapshot = snapshot;
    identity = snapshot.unifiedAddress;
    element("balance-panel").hidden = false;
    element("activity-panel").hidden = false;
    confirmationPolicy = snapshot.confirmations;
    pendingPayment = (await wallet.pending(1)).length > 0;
    showRuntime();
    element("receive-panel").hidden = false;
    hasScanned = (snapshot.scannedHeight ?? 0) >= snapshot.birthdayHeight;
    copyAddress.hidden = false;
    element("scan-panel").hidden = false;
    element("scan-details").textContent = `${connection.network} · ${server} · Birthday ${snapshot.birthdayHeight.toLocaleString()} · ${hasScanned ? "Scanned through " + snapshot.scannedHeight!.toLocaleString() : "Not scanned yet"}`;
    element("send-panel").hidden = false;
    if (address.textContent !== snapshot.unifiedAddress) drawReceiveQr(element<HTMLCanvasElement>("receive-qr"), snapshot.unifiedAddress);
    address.textContent = snapshot.unifiedAddress;
    if (receipt) {
      receipt = await refreshReceipt(wallet, receipt);
      showReceipt(receipt);
    }
    showBalance(BigInt(snapshot.balance.totalAvailable), pendingFunds(snapshot), snapshot.confirmations);
    const entries = await wallet.history(20);
    const rows = entries.map(entry => {
      const row = document.createElement("li");
      const item = classifyHistory(entry);
      const movement = document.createElement("strong"), confirmations = document.createElement("span"), txid = document.createElement("code");
      movement.textContent = activityMovement(entry, unit);
      confirmations.textContent = confirmationLabel(entry, snapshot);
      txid.textContent = entry.txid;
      row.append(movement, confirmations, txid);
      for (const memo of item.memos) {
        const text = document.createElement("p"); text.className = "memo"; text.textContent = memo; row.append(text);
      }
      return row;
    });
    history.replaceChildren(...rows);
    element("history-empty").hidden = rows.length > 0;
    element("history-empty").textContent = hasScanned ? "No activity yet." : "Sync to look for activity.";
    updateControls();
    return entries;
  }

  function clearPhrase() {
    phrase.replaceChildren();
    recoveryPhrase = "";
    copyPhrase.hidden = true;
    element("phrase-copy-status").textContent = "";
    element("phrase-copy-status").hidden = true;
    restoreForm.hidden = !!identity || creating;
    phrase.hidden = true;
    element("backup-panel").hidden = true;
    hidePhrase.hidden = true;
    words.value = "";
    sendWords.value = "";
    sendWords.removeAttribute("aria-invalid");
  }

  function updateControls() {
    showRuntime();
    element("create-panel").hidden = !!identity || creating;
    restoreForm.hidden = !!identity || creating;
    element("remove-panel").hidden = !identity;
    const removalBlocked = pendingPayment || receipt?.state === "pending" || receipt?.state === "unknown";
    remove.disabled = busy || !identity || !removeConfirm.checked || removalBlocked;
    element("remove-pending").hidden = !removalBlocked;
    create.disabled = busy || !!identity;
    sync.disabled = busy || !identity;
    lock.disabled = busy || !identity;
    element<HTMLButtonElement>("review-send").disabled = busy || !identity || !hasScanned;
    element("send-readiness").hidden = hasScanned;
    copyAddress.disabled = busy || !identity;
    element<HTMLButtonElement>("load-details").disabled = busy || !identity;
  }

  async function copy(text: string, output: HTMLElement, secret = false) {
    // Clipboard writes are explicit user actions; no secret is copied automatically.
    try {
      await navigator.clipboard.writeText(text);
      output.textContent = secret ? "Phrase copied. Your clipboard now contains your recovery words." : "Address copied.";
    } catch { output.textContent = secret ? "Could not copy. Save the numbered words in order." : "Could not copy. Select the address instead."; }
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

  async function run(action: () => Promise<void>, output: HTMLElement = status) {
    restoreValidation = "";
    try { await perform(action); }
    catch (error) {
      const safe = WalletError.fromUnknown(error);
      console.warn("Wallet action failed", { code: safe.code });
      output.textContent = output === sendStatus ? paymentErrorMessage(safe, latestSnapshot, unit) : safe.userMessage();
      if (output === sendStatus) {
        const fields: Partial<Record<WalletError["code"], string>> = { invalid_address: "send-to", invalid_amount: "send-amount", invalid_memo: "send-memo", invalid_recovery_phrase: "send-words", seed_mismatch: "send-words" };
        const field = fields[safe.code];
        if (field) element(field).setAttribute("aria-invalid", "true");
      }
    }
  }
  let disposeBase = attachBase(wallet, perform, () => identity);

  function showReceipt(value: SendReceipt) {
    if (value.state === "mined") sendStatus.textContent = "Payment confirmed.";
    if (value.state === "expired") sendStatus.textContent = "Payment expired. Check your balance before another payment.";
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
  sendForm.addEventListener("reset", () => {
    sendStatus.textContent = "";
    for (const field of sendForm.querySelectorAll("[aria-invalid]")) field.removeAttribute("aria-invalid");
  });
  element("confirm-send-form").addEventListener("input", event => {
    if (event.target instanceof HTMLElement) event.target.removeAttribute("aria-invalid");
    sendStatus.textContent = "";
  });
  sendForm.addEventListener("input", event => {
    if (event.target instanceof HTMLElement) event.target.removeAttribute("aria-invalid");
    sendStatus.textContent = ""; status.textContent = "";
  });
  sendForm.addEventListener("invalid", event => {
    if (event.target instanceof HTMLElement) event.target.setAttribute("aria-invalid", "true"); sendStatus.textContent = "Complete the required payment fields."; status.textContent = ""; }, true);
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
    }, sendStatus);
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
        const safe = WalletError.fromUnknown(error);
        sendStatus.textContent = paymentErrorMessage(safe, latestSnapshot, unit);
        if (safe.code === "invalid_recovery_phrase" || safe.code === "seed_mismatch") sendWords.setAttribute("aria-invalid", "true");
      } finally { wallet.lock(); sending = undefined; cancelSend.hidden = true; }
    }, sendStatus);
  });
  element("edit-send").addEventListener("click", () => {
    review = undefined; sendWords.value = ""; sendWords.removeAttribute("aria-invalid"); sendStatus.textContent = ""; reviewPanel.hidden = true; sendForm.hidden = false;
    element("send-to").focus();
  });
  cancelSend.addEventListener("click", () => {
    sending?.abort(); cancelSend.hidden = true; sendStatus.textContent = "Stopping before submission · waiting for proof cleanup…";
  });
  element("another-send").addEventListener("click", () => {
    receipt = undefined; sendForm.reset(); receiptPanel.hidden = true; sendForm.hidden = false; sendStatus.textContent = "";
  });

  createBirthday.addEventListener("input", () => createBirthday.setCustomValidity(""));
  create.addEventListener("click", () => {
    const input = createBirthday.value.trim();
    const creationBirthday = input ? Number(input) : "auto";
    try {
      if (input && !/^\d+$/.test(input)) throw new WalletError("invalid_birthday", "Create needs a block height.");
      validateBirthdayInput(creationBirthday);
    } catch (error) {
      createBirthday.setCustomValidity(WalletError.fromUnknown(error).userMessage());
      createBirthday.reportValidity(); return;
    }
    void run(async () => {
      if (await wallet.load()) {
        status.textContent = "A wallet is already saved here. Back it up before replacing it.";
        return;
      }
      creating = true;
      try {
        const created = await wallet.create({ birthday: creationBirthday, beforeCommit: preparation => {
          // No wallet snapshot exists yet. Leaving now cancels creation rather
          // than storing a wallet whose recovery phrase has never been saved.
          recoveryPhrase = preparation.recoveryPhrase;
          phrase.replaceChildren(...recoveryPhrase.split(/\s+/).map(word => {
            const item = document.createElement("li"); item.textContent = word + " "; return item;
          }));
          restoreForm.hidden = true;
          element("create-panel").hidden = true;
          copyPhrase.hidden = false;
          element("phrase-copy-status").hidden = false;
          phrase.hidden = false;
          element("backup-panel").hidden = false;
          element("backup-panel").focus();
          hidePhrase.hidden = false;
          status.textContent = "Save these words, then finish creating your wallet.";
          return new Promise<void>((resolve, reject) => {
            const cleanup = () => { confirmRecovery = undefined; cancelRecovery = undefined; preparation.signal.removeEventListener("abort", cancel); clearPhrase(); };
            const cancel = () => { cleanup(); reject(new DOMException("Creation cancelled", "AbortError")); };
            confirmRecovery = () => { cleanup(); status.textContent = "Saving wallet…"; resolve(); };
            cancelRecovery = cancel;
            preparation.signal.addEventListener("abort", cancel, { once: true });
            if (preparation.signal.aborted) cancel();
          });
        } });
        await render(created.wallet);
        status.textContent = "Wallet created. Sync when ready.";
      } catch (error) {
        const safe = WalletError.fromUnknown(error);
        if (creationBirthday === "auto" && safe.code === "transport") {
          status.textContent = "Automatic birthday needs the light server. Retry, or enter a known birthday height to create offline.";
        } else { throw error; }
      } finally { creating = false; clearPhrase(); }
    });
  });

  sync.addEventListener("click", () => void run(async () => {
    // Retain the previous balance and history while syncing.
    const snapshot = await wallet.sync();
    await render(snapshot);
    status.textContent = syncedWalletStatus(snapshot);
  }));

  element("load-details").addEventListener("click", () => void run(async () => {
    const snapshot = await wallet.fetchMemos();
    const entries = await render(snapshot);
    status.textContent = memoDetailsMessage(snapshot, entries);
  }));

  restoreForm.addEventListener("submit", event => {
    event.preventDefault();
    const mnemonic = words.value.trim();
    const restoreBirthday = birthday.value.trim();
    try { validateBirthdayInput(restoreBirthday); }
    catch (error) { const message = WalletError.fromUnknown(error).userMessage(); showRestoreValidation(message); birthday.setCustomValidity(message); birthday.setAttribute("aria-invalid", "true"); birthday.reportValidity(); return; }
    void run(async () => {
      // Only restore into an empty local slot. Removal has its own confirmation.
      if (await wallet.load()) {
        status.textContent = "A wallet is already saved here. Save your recovery phrase before removing it to try recovery.";
        return;
      }
      // Keep the input available if validation, transport or persistence fails.
      let restored: WalletSnapshot;
      try { restored = await wallet.restore(mnemonic, { birthday: restoreBirthday }); }
      catch (error) {
        const safe = WalletError.fromUnknown(error);
        restoreValidation = safe.userMessage();
        if (safe.code === "invalid_recovery_phrase" || safe.code === "seed_mismatch") words.setAttribute("aria-invalid", "true");
        throw error;
      }
      clearPhrase();
      await render(restored);
      status.textContent = "Wallet restored. Sync to recover activity.";
    });
  });
  birthday.addEventListener("input", () => { birthday.setCustomValidity(""); birthday.removeAttribute("aria-invalid"); clearRestoreValidation(); });
  words.addEventListener("input", () => { words.removeAttribute("aria-invalid"); clearRestoreValidation(); });
  restoreForm.addEventListener("invalid", event => {
    if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) {
      event.target.setAttribute("aria-invalid", "true");
      showRestoreValidation(event.target.validationMessage);
    }
  }, true);
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
  function clearLocalWalletView() {
    latestSnapshot = undefined; identity = undefined; hasScanned = false; pendingPayment = false; review = undefined; receipt = undefined;
    disposeBase(); disposeBase = attachBase(wallet, perform, () => identity);
    clearPhrase(); restoreForm.reset(); birthday.setCustomValidity("");
    createBirthday.value = ""; createBirthday.setCustomValidity("");
    sendForm.reset(); reviewPanel.hidden = true; receiptPanel.hidden = true; sendForm.hidden = false;
    sendStatus.textContent = ""; element("copy-status").textContent = "";
    element("remove-status").textContent = ""; removeConfirm.checked = false;
    address.textContent = ""; balance.textContent = `— ${unit}`; history.replaceChildren();
    for (const id of ["pending-balance", "pending-help", "send-confirming"]) element(id).hidden = true;
    for (const id of ["receive-panel", "scan-panel", "send-panel", "balance-panel", "activity-panel"]) element(id).hidden = true;
    element("history-empty").hidden = false;
    element("history-empty").textContent = "Create or restore a wallet to see activity.";
    element<HTMLDetailsElement>("remove-panel").open = false;
    status.textContent = "Local wallet removed. You can create or restore.";
  }
  removeConfirm.addEventListener("change", updateControls);
  element<HTMLFormElement>("remove-form").addEventListener("submit", event => {
    event.preventDefault();
    if (!identity || !removeConfirm.checked) return;
    void run(async () => {
      // Fast UI check; the SDK below checks durable reservations and conditionally deletes.
      if ((await wallet.pending(1)).length || receipt?.state === "pending" || receipt?.state === "unknown") {
        pendingPayment = true;
        element("remove-status").textContent = "Sync to confirm or expire pending payments before removing this wallet.";
        return;
      }
      try { await wallet.forget({ passkey: true, pending: "reject" }); }
      catch (error) {
        const safe = WalletError.fromUnknown(error);
        if (safe.code === "wallet_changed" || safe.code === "forget_pending") {
          const saved = await wallet.load();
          if (saved) await render(saved);
          else { clearLocalWalletView(); return; }
        }
        throw error;
      }
      clearLocalWalletView();
    }, element("remove-status"));
  });
  lock.addEventListener("click", () => {
    wallet.lock(); clearPhrase(); status.textContent = "Spending locked. History stays visible.";
  });
  hidePhrase.addEventListener("click", () => confirmRecovery?.());
  clearWords.addEventListener("click", () => { clearPhrase(); birthday.setCustomValidity(""); birthday.removeAttribute("aria-invalid"); words.removeAttribute("aria-invalid"); clearRestoreValidation(); });
  window.addEventListener("beforeunload", event => {
    if (creating) { event.preventDefault(); event.returnValue = ""; }
  });

  window.addEventListener("pagehide", () => {
    disposeBase(); cancelRecovery?.(); wallet.lock();
    clearPhrase();
    words.value = "";
    sending?.abort();
    // Keep the scanner alive for the SDK's best-effort hide save and bfcache.
    // close() belongs to an explicit app teardown while the document is alive.
  });
  window.addEventListener("pageshow", event => {
    if (!event.persisted) return;
    disposeBase = attachBase(wallet, perform, () => identity);
    // The pending action owns the view refresh. Do not replace its status with
    // a busy refusal on return from the back/forward cache.
    if (busy) return;
    void run(async () => { const saved = await wallet.load(); if (saved) await render(saved); else clearLocalWalletView(); });
  });

  await run(async () => {
    const saved = await wallet.load();
    if (saved) await render(saved);
    status.textContent = saved ? loadedWalletStatus(saved) : "Create a wallet or restore one.";
  });
}

void start().catch(error => {
  const safe = WalletError.fromUnknown(error);
  // Never log raw errors: providers may include addresses, keys or request bodies.
  console.error("Wallet startup failed", { code: safe.code });
  status.textContent = `${safe.userMessage()} Reload to try again.`;
  for (const button of [create, sync, restore, lock]) button.disabled = true;
});
