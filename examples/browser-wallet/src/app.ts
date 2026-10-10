import { SDK_VERSION, createWallet, WalletError } from "@z-stack/sdk";
import buildInfo from "../sdk-build.json";
import { connection } from "./connection";
import { element } from "./dom";
import { engineLoading } from "./engine-loading";
import { loadedWalletStatus, paymentErrorMessage } from "./wallet-view";
import type { WalletApp } from "./app-context";
import { attachBase } from "./base";
import { attachScreen } from "./screen";
import { attachRecovery } from "./recovery";
import { attachCreate } from "./create";
import { attachRestore } from "./restore";
import { attachPayments } from "./payment";
import { attachSync } from "./sync";
import { attachRemove } from "./remove";

const sdkBuild: { version: string; revision: string | null } = buildInfo;

export async function startApp() {
  element("sdk-build").textContent = `SDK ${SDK_VERSION} · ${sdkBuild.version === SDK_VERSION && sdkBuild.revision
    ? "source " + sdkBuild.revision.slice(0, 12) : "local archives"}`;
  const wallet = await createWallet({
    ...connection,
    memoFetch: "on-demand",
    autoShield: false,
    unlockPolicy: "each-spend",
    // This example makes network work explicit; production apps may enable autoSync.
    autoSync: false,
    threads: 2,
    onLoadProgress: engineLoading(element("engine-progress"), element("engine-progress-bar", HTMLProgressElement)),
  });
  const unit = connection.network === "testnet" ? "TAZ" : "ZEC";
  const status = element("status");
  const sendStatus = element("send-status");
  element("title").textContent = `${connection.network[0].toUpperCase()}${connection.network.slice(1)} wallet`;
  element("balance").textContent = `— ${unit}`;
  element("send-panel").setAttribute("aria-label", `Send ${unit}`);
  for (const node of document.querySelectorAll("h2, label")) {
    const text = node.textContent;
    if (text && /ZEC|TAZ/.test(text)) node.textContent = text.replace(/ZEC|TAZ/g, unit);
  }
  element("chain-warning").hidden = connection.network !== "testnet";
  const screen = attachScreen(wallet, unit);
  let busy = false;

  const app: WalletApp = {
    wallet, unit,
    get snapshot() { return screen.snapshot; },
    get hasScanned() { return screen.hasScanned(); },
    get unresolvedPayment() { return payments.unresolved; },
    async render(snapshot) {
      const entries = await screen.render(snapshot);
      await payments.refresh();
      updateControls();
      return entries;
    },
    run,
    updateControls,
    clearSecrets() { backup.clear(); restore.clearWords(); payments.clearWords(); },
    reset() {
      screen.clear();
      app.clearSecrets();
      restore.reset();
      creation.reset();
      payments.reset();
      disposeBase();
      disposeBase = attachBase(wallet, perform, () => app.snapshot?.unifiedAddress);
      status.textContent = "Local wallet removed. You can create or restore.";
    },
  };
  const backup = attachRecovery();
  const creation = attachCreate(app, backup);
  const restore = attachRestore(app);
  const payments = attachPayments(app);
  attachSync(app);
  attachRemove(app);
  let disposeBase = attachBase(wallet, perform, () => app.snapshot?.unifiedAddress);

  function updateControls() {
    const exists = app.snapshot !== undefined;
    const removalBlocked = screen.pendingPayment || payments.unresolved;
    element("create-panel").hidden = exists || creation.active;
    element("restore-form").hidden = exists || creation.active;
    element("remove-panel").hidden = !exists;
    element("remove", HTMLButtonElement).disabled = busy || !exists || !element("remove-confirm", HTMLInputElement).checked || removalBlocked;
    element("remove-pending").hidden = !removalBlocked;
    element("create", HTMLButtonElement).disabled = busy || exists;
    for (const id of ["sync", "lock", "copy-address", "load-details"]) element(id, HTMLButtonElement).disabled = busy || !exists;
    element("review-send", HTMLButtonElement).disabled = busy || !exists || !app.hasScanned;
    element("send-readiness").hidden = app.hasScanned;
  }
  async function perform<T>(action: () => Promise<T>): Promise<T> {
    if (busy) throw new Error("Another wallet action is in progress.");
    busy = true;
    const allowed = [element("hide-phrase"), element("cancel-send"), element("cancel-sync"), element("copy-phrase")];
    const controls = [...document.querySelectorAll("input, textarea, button")].filter(node => !allowed.some(control => control === node));
    for (const control of controls) {
      if (control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement || control instanceof HTMLButtonElement) control.disabled = true;
    }
    try { return await action(); }
    finally {
      for (const control of controls) {
        if (control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement || control instanceof HTMLButtonElement) control.disabled = false;
      }
      busy = false;
      updateControls();
    }
  }
  /** Another tab committed first: show what is saved now, including a removal. */
  async function showSavedWallet() {
    const saved = await perform(async () => {
      const current = await wallet.load();
      if (current) await app.render(current);
      return current;
    }).catch(() => undefined);
    if (saved === null) {
      app.reset();
      updateControls();
      status.textContent = "This wallet was removed in another tab. Create or restore to continue.";
    }
    return saved;
  }
  async function run(action: () => Promise<void>, output: HTMLElement = status) {
    try { await perform(action); }
    catch (error) {
      const safe = WalletError.fromUnknown(error);
      console.warn("Wallet action failed", { code: safe.code });
      if (safe.code === "wallet_changed" && await showSavedWallet() === null) return;
      output.textContent = output === sendStatus ? paymentErrorMessage(error, app.snapshot, unit) : safe.userMessage();
      if (output === sendStatus) {
        const fields: Partial<Record<WalletError["code"], string>> = {
          invalid_address: "send-to", invalid_amount: "send-amount", invalid_memo: "send-memo",
          invalid_recovery_phrase: "send-words", seed_mismatch: "send-words",
        };
        const field = fields[safe.code];
        if (field) element(field).setAttribute("aria-invalid", "true");
      }
    }
  }

  element("lock").addEventListener("click", () => {
    wallet.lock();
    app.clearSecrets();
    status.textContent = "Spending locked. History stays visible.";
  });
  window.addEventListener("beforeunload", event => {
    if (creation.active) { event.preventDefault(); event.returnValue = ""; }
  });
  window.addEventListener("pagehide", () => {
    disposeBase();
    backup.cancel();
    wallet.lock();
    app.clearSecrets();
    payments.abort();
    // Leave the scanner alive for the SDK's hide save and back/forward cache.
    // close() is for explicit app teardown while the page is still alive.
  });
  window.addEventListener("pageshow", event => {
    if (!event.persisted) return;
    disposeBase = attachBase(wallet, perform, () => app.snapshot?.unifiedAddress);
    if (busy) return;
    void run(async () => {
      const saved = await wallet.load();
      if (saved) await app.render(saved);
      else app.reset();
    });
  });
  await run(async () => {
    const saved = await wallet.load();
    if (saved) await app.render(saved);
    status.textContent = saved ? loadedWalletStatus(saved) : "Create a wallet or restore one.";
  });
}
