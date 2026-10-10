import { validateBirthdayInput, WalletError, type WalletSnapshot } from "@z-stack/sdk";
import type { WalletApp } from "./app-context";
import { element } from "./dom";

export function attachRestore(app: WalletApp) {
  const form = element("restore-form", HTMLFormElement);
  const words = element("words", HTMLTextAreaElement);
  const birthday = element("birthday", HTMLInputElement);
  const status = element("status");
  let validation = "";

  function showValidation(message: string) {
    validation = message;
    status.textContent = message;
  }
  function clearValidation() {
    if (validation && status.textContent === validation) status.textContent = "";
    validation = "";
  }
  function clearWords() {
    words.value = "";
    words.removeAttribute("aria-invalid");
  }

  form.addEventListener("submit", event => {
    event.preventDefault();
    const mnemonic = words.value.trim();
    const restoreBirthday = birthday.value.trim();
    try {
      validateBirthdayInput(restoreBirthday);
    } catch (error) {
      const message = WalletError.fromUnknown(error).userMessage();
      showValidation(message);
      birthday.setCustomValidity(message);
      birthday.setAttribute("aria-invalid", "true");
      birthday.reportValidity();
      return;
    }
    void app.run(async () => {
      // Refuse replacement before clearing the only on-screen copy of the words.
      if (await app.wallet.load()) {
        status.textContent = "A wallet is already saved here. Save your recovery phrase before removing it to try recovery.";
        return;
      }
      let restored: WalletSnapshot;
      try {
        restored = await app.wallet.restore(mnemonic, { birthday: restoreBirthday });
      } catch (error) {
        const safe = WalletError.fromUnknown(error);
        validation = safe.userMessage();
        if (safe.code === "invalid_recovery_phrase" || safe.code === "seed_mismatch") words.setAttribute("aria-invalid", "true");
        throw error;
      }
      app.clearSecrets();
      await app.render(restored);
      status.textContent = "Wallet restored. Sync to recover activity.";
    });
  });
  birthday.addEventListener("input", () => {
    birthday.setCustomValidity("");
    birthday.removeAttribute("aria-invalid");
    clearValidation();
  });
  words.addEventListener("input", () => {
    words.removeAttribute("aria-invalid");
    clearValidation();
  });
  form.addEventListener("invalid", event => {
    if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) {
      event.target.setAttribute("aria-invalid", "true");
      showValidation(event.target.validationMessage);
    }
  }, true);
  element("clear-words").addEventListener("click", () => {
    app.clearSecrets();
    birthday.setCustomValidity("");
    birthday.removeAttribute("aria-invalid");
    clearValidation();
  });
  return {
    clearWords,
    reset() { form.reset(); birthday.setCustomValidity(""); clearValidation(); },
  };
}
