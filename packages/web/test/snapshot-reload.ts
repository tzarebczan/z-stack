import type { WalletSnapshot } from "@z-stack/core";
import { clearSavedSnapshot, openWalletDb, peekSavedSnapshot, readSavedSnapshot, saveWalletSnapshot } from "../../sdk/src/snapshot-storage";

const marker = "z-stack.synthetic-snapshot-reload";
const size = 40_000_000;
const writes = 160;
const checksum = async (bytes: Uint8Array<ArrayBuffer>) => Array.from(
  new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)), value => value.toString(16).padStart(2, "0"),
).join("");
const output = document.querySelector<HTMLPreElement>("#results")!;
const button = document.querySelector<HTMLButtonElement>("#run")!;
const report = (value: unknown) => { output.textContent = JSON.stringify(value, null, 2); };
const preview: WalletSnapshot = {
  network: "regtest", server: "", birthdayHeight: 1, scannedHeight: 1,
  unifiedAddress: marker, transparentAddress: null, zip321: "", viewOnly: true,
  balance: { saplingAvailable: 0, orchardAvailable: 0, ironwoodAvailable: 0,
    transparentAvailable: 0, totalAvailable: 0, saplingZec: "0", orchardZec: "0",
    ironwoodZec: "0", transparentZec: "0", totalZec: "0" },
};
function check(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
async function run(reload: boolean) {
  button.disabled = true;
  try {
    check(location.hostname === "127.0.0.1" && location.port === "15181", "Use the isolated test origin on port 15181.");
    if (reload) {
      const saved = await peekSavedSnapshot(() => null);
      check(saved?.unifiedAddress === marker && saved.scannedHeight === writes,
        "Saved record is not this completed fixture; nothing was changed.");
      // Do not materialize the final payload before navigation: that can retain
      // a Blob's backing and hide the Chromium failure this test reproduces.
      const bytes = await readSavedSnapshot();
      check(bytes?.length === size && bytes[0] === writes - 1 && bytes.at(-1) === writes - 1,
        "Reloaded snapshot is incomplete or corrupt.");
      check(await checksum(new Uint8Array(bytes)) === sessionStorage.getItem(`${marker}:checksum`), "Reloaded snapshot checksum differs.");
      await clearSavedSnapshot();
      sessionStorage.removeItem(marker);
      sessionStorage.removeItem(`${marker}:checksum`);
      report({ status: "passed", writes, snapshotBytes: size, reloaded: true, userAgent: navigator.userAgent });
      return;
    }
    const db = await openWalletDb();
    try {
      const keys = await new Promise<IDBValidKey[]>((resolve, reject) => {
        const tx = db.transaction("wallets", "readonly");
        const request = tx.objectStore("wallets").getAllKeys();
        tx.oncomplete = () => resolve(request.result);
        tx.onabort = () => reject(tx.error);
      });
      check(keys.every(key => key === "wallet:generation"), "Existing data found; use a new isolated profile.");
    } finally { db.close(); }
    let key: string | null = null;
    // Constant-filled fixtures compress enough to conceal large-value failures.
    const bytes = new Uint8Array(size);
    for (let offset = 0; offset < size; offset += 65_536) {
      crypto.getRandomValues(bytes.subarray(offset, Math.min(offset + 65_536, size)));
    }
    for (let i = 0; i < writes; i++) {
      bytes[0] = i;
      bytes[size - 1] = i;
      key = (await saveWalletSnapshot(bytes,
        { ...preview, scannedHeight: i + 1 }, () => true, undefined, key)) ?? null;
      check(key, "Snapshot write did not commit.");
      if ((i + 1) % 20 === 0) report({ status: "writing", complete: i + 1, writes });
    }
    sessionStorage.setItem(`${marker}:checksum`, await checksum(bytes));
    sessionStorage.setItem(marker, "reload");
    location.reload();
  } catch (error) {
    // Keep a failed synthetic record for diagnosis; never clear an unknown wallet.
    report({ status: "failed", message: String(error) });
  } finally { button.disabled = false; }
}
button.onclick = () => void run(false);
if (sessionStorage.getItem(marker) === "reload") void run(true);
