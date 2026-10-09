import { createWallet, formatZatoshis, WalletError, type WalletSnapshot } from "@z-stack/sdk";
import { createPasskeyVault, indexedDbVaultStore, isPasskeyCancel, PasskeyError } from "@z-stack/sdk/services";
import "./style.css";

const element = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = element("status");
const phrase = element("phrase");
const name = element<HTMLInputElement>("wallet-name");
const protect = element<HTMLButtonElement>("protect");
const unlock = element<HTMLButtonElement>("unlock");
const create = element<HTMLButtonElement>("create");
const sync = element<HTMLButtonElement>("sync");
const lock = element<HTMLButtonElement>("lock");
const hide = element<HTMLButtonElement>("hide-phrase");
const vault = createPasskeyVault({ rpName: "Example Wallet", id: "testnet-wallet",
  purpose: "wallet-seed", store: indexedDbVaultStore({ dbName: "example-passkey-vault" }), portable: false });
const abort = new AbortController();
let seed = "";
let busy = false;
let closed = false;
let saved = false;
let protectedHere = false;
let creating = false;
let confirmCreation: (() => void) | undefined;

function controls() {
  create.disabled = busy || saved || protectedHere;
  sync.disabled = lock.disabled = busy || !saved;
  protect.disabled = name.disabled = busy || !seed || protectedHere;
  unlock.disabled = busy || !saved || !protectedHere;
  hide.hidden = !seed;
  hide.textContent = confirmCreation ? "I saved these words — finish" : "Hide recovery phrase";
}
function clearPhrase() { seed = ""; phrase.textContent = ""; phrase.hidden = true; controls(); }

async function start() {
  const wallet = await createWallet({ network: "testnet", server: "https://zcash-testnet.chainsafe.dev",
    memoFetch: "on-demand", autoShield: false, autoSync: false, unlockPolicy: "each-spend" });
  if (closed) { await wallet.close(); return; }
  const off = wallet.on("sync", progress => {
    if (!closed) status.textContent = progress.stage === "synced" ? "Up to date" : "Syncing…";
  });
  async function render(snapshot: WalletSnapshot) {
    if (closed) return;
    element("address").textContent = snapshot.unifiedAddress;
    element("balance").textContent = `${formatZatoshis(BigInt(snapshot.balance?.totalAvailable ?? 0))} ZEC`;
    const entries = await wallet.history(20);
    if (closed) return;
    element("history").replaceChildren(...entries.map(entry => {
      const row = document.createElement("li"); row.textContent = entry.txid; return row;
    }));
  }
  // The callback starts synchronously in the click: don't await setup before WebAuthn.
  async function run(action: () => Promise<void>) {
    if (busy || closed) return;
    busy = true; controls();
    try { await action(); }
    catch (error) {
      if (!closed) status.textContent = isPasskeyCancel(error) ? "Cancelled." :
        error instanceof WalletError ? error.userMessage() :
        error instanceof PasskeyError ? "Passkey unavailable. Keep your recovery phrase; try a PRF-capable provider." :
        "Could not complete action.";
    } finally { busy = false; if (!closed) controls(); }
  }
  create.addEventListener("click", () => void run(async () => {
    creating = true;
    let committed = false;
    try {
      const created = await wallet.create({ birthday: "auto", beforeCommit: preparation => {
        if (closed) throw new DOMException("Creation cancelled", "AbortError");
        seed = preparation.recoveryPhrase;
        phrase.textContent = seed; phrase.hidden = false;
        status.textContent = "Save these words, then finish creating your wallet.";
        return new Promise<void>((resolve, reject) => {
          const cleanup = () => { confirmCreation = undefined; preparation.signal.removeEventListener("abort", cancel); controls(); };
          const cancel = () => { cleanup(); clearPhrase(); reject(new DOMException("Creation cancelled", "AbortError")); };
          confirmCreation = () => { cleanup(); status.textContent = "Saving wallet…"; resolve(); };
          preparation.signal.addEventListener("abort", cancel, { once: true });
          controls();
          if (preparation.signal.aborted) cancel();
        });
      } });
      committed = true;
      if (closed) return;
      saved = true;
      status.textContent = "Wallet created. Add a passkey if you want one.";
      await render(created.wallet);
    } finally { creating = false; if (!committed) clearPhrase(); }

  }));
  protect.addEventListener("click", () => void run(async () => {
    const label = name.value.trim() || "Testnet wallet";
    await vault.protect(seed, { userName: label, userDisplayName: label, name: label, signal: abort.signal });
    if (closed) return;
    protectedHere = true;
    wallet.lock();
    status.textContent = "Passkey saved on this browser. Keep your recovery phrase.";
    // Deliberately retain the phrase until the user hides it; never imply a cloud backup.
  }));
  unlock.addEventListener("click", () => void run(async () => {
    const secret = await vault.unlock({ signal: abort.signal });
    try {
      if (closed) return;
      await wallet.unlock(secret.text());
      if (!closed) status.textContent = "Unlocked. Each spend requires unlocking again.";
    } finally { secret.wipe(); }
  }));
  lock.addEventListener("click", () => { wallet.lock(); clearPhrase(); status.textContent = "Spending locked. History stays visible."; });
  hide.addEventListener("click", () => { if (confirmCreation) confirmCreation(); else clearPhrase(); });
  window.addEventListener("beforeunload", event => { if (creating) { event.preventDefault(); event.returnValue = ""; } });
  sync.addEventListener("click", () => void run(async () => { await render(await wallet.sync()); }));
  window.addEventListener("pagehide", () => {
    closed = true; abort.abort(); off(); clearPhrase();
    // Do not close() here. The SDK saves the snapshot on pagehide, and close()
    // interrupts the scan worker before that save starts.
    wallet.lock();
  }, { once: true });
  await run(async () => {
    const current = await wallet.load();
    protectedHere = await vault.exists(abort.signal);
    saved = !!current;
    if (current) await render(current);
    if (!closed) status.textContent = current ? "Wallet opened. Spending is locked." :
      protectedHere ? "Only the local vault remains. Restore with your phrase in the browser example." : "Create a testnet wallet.";
  });
}
window.addEventListener("pagehide", () => { closed = true; abort.abort(); clearPhrase(); }, { once: true });
void start().catch(() => { if (!closed) status.textContent = "Could not open wallet. Reload to try again."; });
