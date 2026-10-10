import { validateBirthdayInput, WalletError } from "@z-stack/sdk";
import type { WalletApp } from "./app-context";
import { element } from "./dom";
import { memoDetailsMessage, syncedWalletStatus } from "./wallet-view";

export function attachSync(app: WalletApp) {
  const status = element("status");
  app.wallet.on("sync", progress => {
    status.textContent = progress.stage === "synced"
      ? `Synced through block ${progress.scanned?.toLocaleString() ?? "unknown"}. Sync again for payments mined later.`
      : `Syncing · ${Math.round(progress.percent ?? 0)}%`;
  });
  element("sync").addEventListener("click", () => void app.run(async () => {
    // Previous activity and balances stay visible while the SDK scans.
    const snapshot = await app.wallet.sync();
    await app.render(snapshot);
    status.textContent = syncedWalletStatus(snapshot);
  }));
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
      await app.render(await app.wallet.sync());
    });
  });
}
