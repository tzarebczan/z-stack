import { validateBirthdayInput, WalletError } from "@z-stack/sdk";
import type { WalletApp } from "./app-context";
import type { RecoveryBackup } from "./recovery";
import { element } from "./dom";

export function attachCreate(app: WalletApp, backup: RecoveryBackup) {
  const birthday = element("create-birthday", HTMLInputElement);
  const status = element("status");
  let active = false;

  birthday.addEventListener("input", () => birthday.setCustomValidity(""));
  element("create").addEventListener("click", () => {
    const input = birthday.value.trim();
    const creationBirthday = input ? Number(input) : "auto";
    try {
      if (input && !/^\d+$/.test(input)) throw new WalletError("invalid_birthday", "Create needs a block height.");
      validateBirthdayInput(creationBirthday);
    } catch (error) {
      birthday.setCustomValidity(WalletError.fromUnknown(error).userMessage());
      birthday.reportValidity();
      return;
    }
    void app.run(async () => {
      if (await app.wallet.load()) {
        status.textContent = "A wallet is already saved here. Back it up before replacing it.";
        return;
      }
      active = true;
      try {
        const created = await app.wallet.create({
          birthday: creationBirthday,
          // The SDK commits only after the user acknowledges their words.
          beforeCommit: preparation => backup.prepare(preparation.recoveryPhrase, preparation.signal),
        });
        await app.render(created.wallet);
        status.textContent = "Wallet created. Sync when ready.";
      } catch (error) {
        const safe = WalletError.fromUnknown(error);
        if (creationBirthday === "auto" && safe.code === "transport") {
          status.textContent = "Automatic birthday needs the light server. Retry, or enter a known birthday height to create offline.";
        } else {
          throw error;
        }
      } finally {
        active = false;
        app.clearSecrets();
      }
    });
  });
  return {
    get active() { return active; },
    reset() { birthday.value = ""; birthday.setCustomValidity(""); },
  };
}
