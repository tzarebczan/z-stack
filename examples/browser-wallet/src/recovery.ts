import { copyText, element } from "./dom";

/** Recovery words live here only until acknowledgement or cancellation. */
export function attachRecovery() {
  const phrase = element("phrase");
  const panel = element("backup-panel");
  const copy = element("copy-phrase", HTMLButtonElement);
  const saved = element("hide-phrase", HTMLButtonElement);
  const copyStatus = element("phrase-copy-status");
  let words = "";
  let acknowledge: (() => void) | undefined;
  let cancel: (() => void) | undefined;

  function clear() {
    words = "";
    phrase.replaceChildren();
    phrase.hidden = true;
    panel.hidden = true;
    copy.hidden = true;
    saved.hidden = true;
    copyStatus.textContent = "";
    copyStatus.hidden = true;
  }

  function prepare(recoveryPhrase: string, signal: AbortSignal): Promise<void> {
    words = recoveryPhrase;
    phrase.replaceChildren(...words.split(/\s+/).map(word => {
      const item = document.createElement("li");
      item.textContent = word + " ";
      return item;
    }));
    element("restore-form").hidden = true;
    element("create-panel").hidden = true;
    for (const control of [phrase, panel, copy, saved, copyStatus]) control.hidden = false;
    panel.focus();
    element("status").textContent = "Save these words, then finish creating your wallet.";

    return new Promise((resolve, reject) => {
      function cleanup() {
        acknowledge = undefined;
        cancel = undefined;
        signal.removeEventListener("abort", abort);
        clear();
      }
      function abort() {
        cleanup();
        reject(new DOMException("Creation cancelled", "AbortError"));
      }
      acknowledge = () => {
        cleanup();
        element("status").textContent = "Saving wallet…";
        resolve();
      };
      cancel = abort;
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) abort();
    });
  }

  saved.addEventListener("click", () => acknowledge?.());
  copy.addEventListener("click", () => {
    if (words) void copyText(words, copyStatus, true);
  });
  return { prepare, clear, cancel: () => cancel?.() };
}

export type RecoveryBackup = ReturnType<typeof attachRecovery>;
