import { validateBirthdayInput, WalletError } from "@z-stack/sdk";
import type { WalletApp } from "./app-context";
import { element } from "./dom";
import { memoDetailsMessage, syncedWalletStatus, syncProgressLabel } from "./wallet-view";

export function attachSync(app: WalletApp) {
  const status = element("status");
  const cancel = element("cancel-sync", HTMLButtonElement);
  app.wallet.on("sync", progress => { status.textContent = syncProgressLabel(progress); });

  /** Cancel keeps committed progress; render it rather than the pre-sync view. */
  async function cancellable(scan: () => Promise<void>) {
    cancel.hidden = false;
    try {
      await scan();
    } catch (error) {
      if (WalletError.fromUnknown(error).code !== "cancelled") throw error;
      await app.render(await app.wallet.getWallet());
      status.textContent = "Sync stopped. Scanned blocks are saved; Sync resumes from there.";
    } finally {
      cancel.hidden = true;
    }
  }
  cancel.addEventListener("click", () => app.wallet.cancelSync());

  element("sync").addEventListener("click", () => void app.run(() => cancellable(async () => {
    // Previous activity and balances stay visible while the SDK scans.
    const snapshot = await app.wallet.sync();
    await app.render(snapshot);
    status.textContent = syncedWalletStatus(snapshot);
  })));
  element("load-details").addEventListener("click", () => void app.run(async () => {
    const snapshot = await app.wallet.fetchMemos();
    const entries = await app.render(snapshot);
    status.textContent = memoDetailsMessage(snapshot, entries);
  }));

  const birthday = element("rescan-birthday", HTMLInputElement);
  birthday.addEventListener("input", () => birthday.setCustomValidity(""));
  element("rescan-form", HTMLFormElement).addEventListener("submit", event => {
    event.preventDefault();
    try {
      validateBirthdayInput(birthday.value);
    } catch (error) {
      birthday.setCustomValidity(WalletError.fromUnknown(error).userMessage());
      birthday.reportValidity();
      return;
    }
    element("rescan-confirm", HTMLInputElement).checked = false;
    void app.run(async () => {
      app.wallet.lock();
      await app.render(await app.wallet.rescan({ birthday: birthday.value.trim() }));
      await cancellable(async () => { await app.render(await app.wallet.sync()); });
    });
  });
}
