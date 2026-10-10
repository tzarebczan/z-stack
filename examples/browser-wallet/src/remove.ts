import { WalletError } from "@z-stack/sdk";
import type { WalletApp } from "./app-context";
import { element } from "./dom";

export function attachRemove(app: WalletApp) {
  const confirmation = element("remove-confirm", HTMLInputElement);
  const status = element("remove-status");
  confirmation.addEventListener("change", app.updateControls);
  element("remove-form", HTMLFormElement).addEventListener("submit", event => {
    event.preventDefault();
    if (!app.snapshot || !confirmation.checked) return;
    void app.run(async () => {
      if ((await app.wallet.pending(1)).length || app.unresolvedPayment) {
        await app.render(await app.wallet.getWallet());
        status.textContent = "Sync to confirm or expire pending payments before removing this wallet.";
        return;
      }
      try {
        // The durable SDK guard also checks for another tab's newer snapshot.
        await app.wallet.forget({ passkey: true, pending: "reject" });
      } catch (error) {
        const safe = WalletError.fromUnknown(error);
        if (safe.code === "wallet_changed" || safe.code === "forget_pending") {
          const saved = await app.wallet.load();
          if (saved) await app.render(saved);
          else { app.reset(); return; }
        }
        throw error;
      }
      app.reset();
    }, status);
  });
}
